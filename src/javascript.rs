#![allow(dead_code)]

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator};

const ECMASCRIPT_QUERY: &str = include_str!("../queries/ecmascript.scm");
const TYPESCRIPT_QUERY: &str = include_str!("../queries/typescript.scm");
const JSX_QUERY: &str = include_str!("../queries/jsx.scm");
const SIGNATURE_LIMIT: usize = 200;

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
        let language: Language = match dialect {
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

    fn parse(&mut self, dialect: ScriptDialect, source: &str) -> Result<ParsedFile, String> {
        let Some(tree) = self.parser.parse(source, None) else {
            return Ok(ParsedFile::default());
        };
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
                    if let Some(definition) = definition(capture.node, source) {
                        parsed.definitions.push(definition);
                    }
                }
                "module" | "typescript_module" => collect_module(capture.node, source, &mut parsed),
                "binding" => collect_binding(capture.node, source, &mut parsed.bindings),
                "jsx" => collect_jsx(capture.node, source, &mut parsed.jsx),
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
                        && definition.range.start <= parsed.definitions[child].range.start
                        && parsed.definitions[child].range.end <= definition.range.end
                })
                .min_by_key(|(_, definition)| definition.range.end - definition.range.start)
                .map(|(parent, _)| parent);
        }
        for capture in &captured {
            if capture.name == "call"
                && let Some(call) = call(capture.node, source)
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
        parser.parse(dialect, source)
    }
}

#[derive(Clone, Copy)]
struct ByteRange {
    start: usize,
    end: usize,
}

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
}

struct ModuleStatement {
    module: String,
}

struct LexicalBinding {
    name: String,
    range: ByteRange,
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
    jsx: Vec<String>,
}

fn definition(node: Node<'_>, source: &str) -> Option<Definition> {
    let (kind, name) = match node.kind() {
        "class_declaration" | "abstract_class_declaration" => (
            DefinitionKind::Type {
                runtime_value: true,
            },
            field_text(node, "name", source)?,
        ),
        "interface_declaration" | "type_alias_declaration" => (
            DefinitionKind::Type {
                runtime_value: false,
            },
            field_text(node, "name", source)?,
        ),
        "enum_declaration" | "internal_module" | "module" => (
            DefinitionKind::Type {
                runtime_value: true,
            },
            field_text(node, "name", source)?,
        ),
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            (DefinitionKind::Function, field_text(node, "name", source)?)
        }
        "method_definition" | "method_signature" | "abstract_method_signature" => {
            (DefinitionKind::Method, field_text(node, "name", source)?)
        }
        "function_expression" | "generator_function" | "arrow_function" | "class" => {
            let (name, kind) = stable_initializer(node, source)?;
            (kind, name)
        }
        _ => return None,
    };
    let body = node.child_by_field_name("body").unwrap_or(node);
    Some(Definition {
        kind,
        name: name.to_owned(),
        parent: None,
        line_start: line_start(node),
        line_end: line_end(node),
        signature: signature(node, source),
        body: ByteRange {
            start: body.start_byte(),
            end: body.end_byte(),
        },
        range: ByteRange {
            start: node.start_byte(),
            end: node.end_byte(),
        },
    })
}

fn stable_initializer<'source>(
    node: Node<'_>,
    source: &'source str,
) -> Option<(&'source str, DefinitionKind)> {
    let parent = node.parent()?;
    if parent.kind() == "variable_declarator"
        && parent
            .child_by_field_name("value")
            .is_some_and(|value| value.id() == node.id())
    {
        return Some((
            field_text(parent, "name", source)?,
            DefinitionKind::Function,
        ));
    }
    if parent.kind() == "field_definition"
        && parent
            .child_by_field_name("value")
            .is_some_and(|value| value.id() == node.id())
    {
        return Some((
            field_text(parent, "property", source)?,
            DefinitionKind::Method,
        ));
    }
    None
}

fn collect_module(node: Node<'_>, source: &str, parsed: &mut ParsedFile) {
    if node.kind() != "import_statement" {
        return;
    }
    let Some(module) = node
        .child_by_field_name("source")
        .map(|source_node| text(source_node, source))
    else {
        return;
    };
    let module = module.trim_matches(['\'', '"']);
    if module.starts_with("./") || module.starts_with("../") {
        parsed.modules.push(ModuleStatement {
            module: module.into(),
        });
    }
}

fn collect_binding(node: Node<'_>, source: &str, bindings: &mut Vec<LexicalBinding>) {
    let target = match node.kind() {
        "formal_parameters" => node,
        "variable_declarator" => node.child_by_field_name("name").unwrap_or(node),
        "catch_clause" => node.child_by_field_name("parameter").unwrap_or(node),
        _ => return,
    };
    let mut pending = vec![target];
    while let Some(identifier) = pending.pop() {
        if identifier.kind() == "identifier" {
            bindings.push(LexicalBinding {
                name: text(identifier, source).to_owned(),
                range: ByteRange {
                    start: target.start_byte(),
                    end: target.end_byte(),
                },
            });
            continue;
        }
        let mut cursor = identifier.walk();
        pending.extend(identifier.named_children(&mut cursor));
    }
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

fn collect_jsx(node: Node<'_>, source: &str, jsx: &mut Vec<String>) {
    if node.kind() == "identifier" || node.kind() == "member_expression" {
        let name = text(node, source);
        if name.chars().next().is_some_and(char::is_uppercase) {
            jsx.push(name.to_owned());
        }
    }
}

fn signature(node: Node<'_>, source: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte());
    let text = source
        .get(node.start_byte()..end)
        .unwrap_or_default()
        .trim_end();
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
        Vec::new()
    }

    fn test_names(&self) -> Vec<&str> {
        Vec::new()
    }

    fn jsx_component_names(&self) -> Vec<&str> {
        self.jsx.iter().map(String::as_str).collect()
    }
}

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
