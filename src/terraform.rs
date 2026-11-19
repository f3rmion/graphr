use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::serde_json;
use tree_sitter::{Language, Node, Parser};

use crate::git::{Language as SourceLanguage, Source};
use crate::parse::{source_notes, syntax_gaps};
use crate::store::{
    GapCategory, GapInput, GapReason, Graph, NodeInput, NodeKind, RefInput, RefKind,
    ResolutionState, SourceNoteInput,
};

const MAX_EXPRESSION_DEPTH: usize = 96;
const MODULE_SOURCE_KEY: &str = "terraform-source:";
const PROVIDER_SOURCE_KEY: &str = "terraform-provider-source:";
const PROVIDER_CONFIG_KEY: &str = "terraform-provider-configuration";
const PROVIDER_REFERENCE_KEY: &str = "terraform-provider-reference:";
const MAX_MODULE_DEPTH: usize = 16;
const MAX_CHILD_RECORDS: usize = 250_000;
const MAX_CHILD_BYTES: usize = 64 * 1024 * 1024;
const MODULE_CONTROL_ARGUMENTS: [&str; 3] = ["count", "for_each", "depends_on"];

#[derive(Default)]
pub(crate) struct TerraformParsers {
    hcl: Option<Parser>,
    json: Option<Parser>,
}

fn parse(
    parser: &mut Option<Parser>,
    language: impl FnOnce() -> Language,
    text: &str,
) -> Result<Option<tree_sitter::Tree>, String> {
    if parser.is_none() {
        let mut new = Parser::new();
        new.set_language(&language())
            .map_err(|error| error.to_string())?;
        *parser = Some(new);
    }
    Ok(parser
        .as_mut()
        .expect("parser initialized")
        .parse(text, None))
}

pub(crate) fn add_file(
    graph: &mut Graph,
    source: &Source,
    parsers: &mut TerraformParsers,
) -> Result<(), String> {
    let directory = module_directory(&source.path);
    let file_key = format!("terraform-file:{}", source.path);
    let file_node = graph.nodes.len();
    let line_end = u32::try_from(source.text.lines().count().max(1))
        .map_err(|_| "Terraform file has too many lines")?;
    graph.nodes.push(NodeInput {
        key: file_key.clone(),
        file_key: source.path.clone(),
        kind: NodeKind::File,
        name: source.path.clone(),
        qualified_name: file_key.clone(),
        parent_key: None,
        owner_key: None,
        line_start: 1,
        line_end,
        signature: String::new(),
        keys: Vec::new(),
    });

    let mut file = TerraformFile {
        graph,
        source,
        directory,
        file_key,
        file_node,
    };
    let json = source.path.ends_with(".tf.json");
    let tree = if json {
        parse(
            &mut parsers.json,
            || tree_sitter_json::LANGUAGE.into(),
            &source.text,
        )?
    } else {
        parse(
            &mut parsers.hcl,
            || tree_sitter_hcl::LANGUAGE.into(),
            &source.text,
        )?
    };
    if let Some(tree) = tree {
        let root = tree.root_node();
        for gap in syntax_gaps(root) {
            file.gap(
                None,
                gap.line_start as u32,
                gap.line_end as u32,
                GapReason::ParserError,
                false,
            );
        }
        if is_override(&source.path) {
            file.gap(None, 1, line_end, GapReason::TerraformOverride, false);
        } else if json {
            if let Some(object) = named_children(root)
                .into_iter()
                .find(|node| node.kind() == "object")
                && !root.has_error()
                && serde_json::from_str::<serde_json::Value>(&source.text).is_ok()
            {
                file.json(object, &mut parsers.hcl)?;
            } else if !root.has_error() {
                file.gap(None, 1, line_end, GapReason::ParserError, false);
            }
        } else {
            file.graph
                .source_notes
                .extend(
                    source_notes(root, &source.text)
                        .into_iter()
                        .map(|note| SourceNoteInput {
                            file_key: source.path.clone(),
                            note,
                        }),
                );
            if let Some(body) = named_children(root)
                .into_iter()
                .find(|node| node.kind() == "body")
            {
                file.hcl(body, &mut parsers.hcl)?;
            } else if !root.has_error() && !named_children(root).is_empty() {
                file.gap(None, 1, line_end, GapReason::ParserError, false);
            }
        }
    } else {
        file.gap(None, 1, line_end, GapReason::ParserNoTree, false);
    }

    Ok(())
}

struct TerraformFile<'a> {
    graph: &'a mut Graph,
    source: &'a Source,
    directory: &'a str,
    file_key: String,
    file_node: usize,
}

impl TerraformFile<'_> {
    fn hcl(&mut self, body: Node<'_>, parser: &mut Option<Parser>) -> Result<(), String> {
        let text = &self.source.text;
        let mut blocks = named_children(body);
        blocks.reverse();
        while let Some(block) = blocks.pop() {
            if block.kind() != "block" {
                continue;
            }
            let children = named_children(block);
            let Some(first) = children.first() else {
                continue;
            };
            let kind = node_text(*first, text);
            let Some(labels) = hcl_labels(block, text) else {
                self.gap(
                    None,
                    line(block),
                    end_line(block),
                    GapReason::ParserError,
                    false,
                );
                continue;
            };
            let body = children.into_iter().find(|child| child.kind() == "body");
            if kind == "terraform" {
                if let Some(body) = body {
                    self.hcl_provider_requirements(body, text);
                }
                continue;
            }
            if kind == "locals" && labels.is_empty() {
                for attr in body.into_iter().flat_map(named_children) {
                    if let Some((name, expression)) = hcl_attribute(attr, text) {
                        let node = self.declaration(attr, format!("local.{name}"), None);
                        self.hcl_expression(&node, expression, text, None, &mut Vec::new(), 0);
                    }
                }
                continue;
            }
            if kind == "import" {
                self.import_gap(block);
                continue;
            }
            let attributes = body.map_or_else(Vec::new, named_children);
            let argument = |name| {
                attributes
                    .iter()
                    .copied()
                    .filter_map(|node| hcl_attribute(node, text))
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value)
            };
            let alias = (kind == "provider").then(|| argument("alias")).flatten();
            let provider_alias = alias
                .and_then(|node| hcl_string(node, text))
                .filter(|alias| terraform_identifier(alias));
            if let Some(alias) = alias
                && provider_alias.is_none()
            {
                self.provider_gap(None, alias);
                continue;
            }
            let Some(address) = declaration_address(kind, &labels, provider_alias.as_deref())
            else {
                continue;
            };
            let source_attr = (kind == "module")
                .then(|| {
                    attributes
                        .iter()
                        .copied()
                        .filter_map(|node| hcl_attribute(node, text))
                        .find(|(name, _)| *name == "source")
                        .map(|(_, node)| node)
                })
                .flatten();
            let literal = source_attr.and_then(|node| hcl_string(node, text));
            let local = literal
                .as_deref()
                .and_then(|path| local_directory(self.directory, path));
            let node = self.declaration(block, address.clone(), local.as_deref());
            if kind == "provider" {
                self.mark_provider_configuration();
            }
            if let Some(resource_type) = resource_type(kind, &labels) {
                self.resource_provider(
                    &node,
                    resource_type,
                    argument("provider"),
                    |value| hcl_provider(value, text),
                    line(block),
                );
            }
            if kind == "module" {
                self.module_boundary(
                    &node,
                    source_attr.unwrap_or(block),
                    literal.as_deref(),
                    local.as_deref(),
                );
            }
            for child in attributes {
                if let Some((name, expression)) = hcl_attribute(child, text) {
                    if kind == "module" && name == "providers" {
                        self.hcl_provider_mappings(&node, &address, expression, text);
                        continue;
                    }
                    let binding = local
                        .as_ref()
                        .and_then(|_| self.module_argument(&node, &address, name, child));
                    let dependent = binding.as_deref().unwrap_or(&node);
                    if name == "depends_on" {
                        self.hcl_depends_on(dependent, expression, text, parser)?;
                    } else if !literal_hcl_argument(kind, name) {
                        self.hcl_expression(dependent, expression, text, None, &mut Vec::new(), 0);
                    }
                } else if kind == "check"
                    && child.kind() == "block"
                    && child
                        .named_child(0)
                        .is_some_and(|name| node_text(name, text) == "data")
                {
                    // Terraform keeps check-scoped data sources in the module's
                    // data namespace, and the check depends on each of them.
                    if let Some(labels) = hcl_labels(child, text)
                        && let [data_type, name] = labels.as_slice()
                    {
                        let address = format!("data.{data_type}.{name}");
                        self.reference(&node, lookup_key(self.directory, &address), line(child));
                    }
                    blocks.push(child);
                } else {
                    self.hcl_expression(&node, child, text, None, &mut Vec::new(), 0);
                }
            }
        }
        Ok(())
    }

    fn json(&mut self, root: Node<'_>, parser: &mut Option<Parser>) -> Result<(), String> {
        let text = &self.source.text;
        let mut blocks = json_pairs(root, text)
            .into_iter()
            .rev()
            .map(|(kind, _, value)| (kind, value))
            .collect::<Vec<_>>();
        while let Some((kind, value)) = blocks.pop() {
            if kind == "terraform" {
                self.json_provider_requirements(value, text);
                continue;
            }
            if kind == "locals" {
                for (name, property, expression) in json_objects(value)
                    .into_iter()
                    .flat_map(|block| json_pairs(block, text))
                {
                    let node = self.declaration(property, format!("local.{name}"), None);
                    self.json_expression(&node, expression, false, &mut Vec::new(), parser)?;
                }
                continue;
            }
            if kind == "import" {
                for block in json_objects(value) {
                    self.import_gap(block);
                }
                continue;
            }
            let label_count = match kind.as_str() {
                "resource" | "data" | "ephemeral" => 2,
                "module" | "variable" | "output" | "check" | "provider" => 1,
                _ => continue,
            };
            let mut pending = vec![(Vec::new(), value, value)];
            while let Some((labels, property, value)) = pending.pop() {
                if value.kind() == "array" {
                    pending.extend(
                        named_children(value)
                            .into_iter()
                            .rev()
                            .map(|entry| (labels.clone(), entry, entry)),
                    );
                } else if value.kind() != "object" {
                    self.gap(
                        None,
                        line(value),
                        end_line(value),
                        GapReason::ParserError,
                        false,
                    );
                } else if labels.len() < label_count {
                    pending.extend(json_pairs(value, text).into_iter().rev().map(
                        |(label, property, entry)| {
                            let mut labels = labels.clone();
                            labels.push(label);
                            (labels, property, entry)
                        },
                    ));
                } else {
                    let args = json_pairs(value, text);
                    let argument = |name| {
                        args.iter()
                            .find(|(key, _, _)| key == name)
                            .map(|(_, _, value)| *value)
                    };
                    let alias = (kind == "provider").then(|| argument("alias")).flatten();
                    let provider_alias = alias
                        .and_then(|node| json_string(node, text))
                        .filter(|alias| terraform_identifier(alias));
                    if let Some(alias) = alias
                        && provider_alias.is_none()
                    {
                        self.provider_gap(None, alias);
                        continue;
                    }
                    let Some(address) =
                        declaration_address(&kind, &labels, provider_alias.as_deref())
                    else {
                        continue;
                    };
                    let source_attr = args
                        .iter()
                        .find(|(name, _, _)| kind == "module" && name == "source")
                        .map(|(_, _, node)| *node);
                    let literal = source_attr.and_then(|node| json_string(node, text));
                    let local = literal
                        .as_deref()
                        .and_then(|path| local_directory(self.directory, path));
                    let node = self.declaration(property, address.clone(), local.as_deref());
                    if kind == "provider" {
                        self.mark_provider_configuration();
                    }
                    if let Some(resource_type) = resource_type(&kind, &labels) {
                        self.resource_provider(
                            &node,
                            resource_type,
                            argument("provider"),
                            |value| json_string(value, text),
                            line(property),
                        );
                    }
                    if kind == "module" {
                        self.module_boundary(
                            &node,
                            source_attr.unwrap_or(property),
                            literal.as_deref(),
                            local.as_deref(),
                        );
                    }
                    for (name, attr, expression) in args {
                        if kind == "module" && name == "providers" {
                            self.json_provider_mappings(&node, &address, expression, text);
                            continue;
                        }
                        if kind == "check" && name == "data" {
                            // Terraform keeps check-scoped data sources in the module's
                            // data namespace, and the check depends on each of them.
                            for (data_type, _, names) in json_objects(expression)
                                .into_iter()
                                .flat_map(|types| json_pairs(types, text))
                            {
                                for (name, property, _) in json_objects(names)
                                    .into_iter()
                                    .flat_map(|names| json_pairs(names, text))
                                {
                                    let address = format!("data.{data_type}.{name}");
                                    self.reference(
                                        &node,
                                        lookup_key(self.directory, &address),
                                        line(property),
                                    );
                                }
                            }
                            blocks.push(("data".to_owned(), expression));
                            continue;
                        }
                        let binding = local
                            .as_ref()
                            .and_then(|_| self.module_argument(&node, &address, &name, attr));
                        if !literal_hcl_argument(&kind, &name) {
                            let dependent = binding.as_deref().unwrap_or(&node);
                            // Module arguments are attributes; other bodies may hold blocks.
                            if kind == "module" {
                                self.json_expression(
                                    dependent,
                                    expression,
                                    name == "depends_on",
                                    &mut Vec::new(),
                                    parser,
                                )?;
                            } else {
                                self.json_argument(
                                    dependent,
                                    &name,
                                    expression,
                                    &mut Vec::new(),
                                    parser,
                                )?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn declaration(&mut self, node: Node<'_>, address: String, local: Option<&str>) -> String {
        let key = format!("{}:{}:{address}", self.file_key, node.start_byte());
        let mut keys = vec![lookup_key(self.directory, &address)];
        if let Some(local) = local {
            keys.push(format!("{MODULE_SOURCE_KEY}{local}"));
        }
        self.graph.nodes.push(NodeInput {
            key: key.clone(),
            file_key: self.source.path.clone(),
            kind: NodeKind::Terraform,
            name: address,
            qualified_name: key.clone(),
            parent_key: Some(self.file_key.clone()),
            owner_key: None,
            line_start: line(node),
            line_end: end_line(node),
            signature: String::new(),
            keys,
        });
        key
    }

    fn module_boundary(
        &mut self,
        node: &str,
        source: Node<'_>,
        literal: Option<&str>,
        local: Option<&str>,
    ) {
        if local.is_none() {
            let reason = if literal.is_some() {
                GapReason::ExternalDependency
            } else {
                GapReason::DynamicOrUnsupportedDispatch
            };
            self.gap(Some(node), line(source), end_line(source), reason, true);
        }
    }

    fn module_argument(
        &mut self,
        node: &str,
        module: &str,
        name: &str,
        argument: Node<'_>,
    ) -> Option<String> {
        if matches!(name, "source" | "version" | "providers") {
            return None;
        }
        let kind = if MODULE_CONTROL_ARGUMENTS.contains(&name) {
            "configuration"
        } else {
            "input"
        };
        let address = format!("{module}.{kind}.{name}");
        let binding = self.declaration(argument, address.clone(), None);
        self.reference(node, lookup_key(self.directory, &address), line(argument));
        Some(binding)
    }

    fn hcl_provider_requirements(&mut self, body: Node<'_>, text: &str) {
        for block in named_children(body) {
            if block.kind() != "block"
                || block
                    .named_child(0)
                    .is_none_or(|name| node_text(name, text) != "required_providers")
            {
                continue;
            }
            for attr in named_children(block)
                .into_iter()
                .filter(|child| child.kind() == "body")
                .flat_map(named_children)
            {
                if let Some((local, value)) = hcl_attribute(attr, text) {
                    for (_, key, source) in hcl_object_pairs(value).unwrap_or_default() {
                        if (node_text(key, text) == "source"
                            || hcl_string(key, text).as_deref() == Some("source"))
                            && let Some(address) = hcl_string(source, text)
                        {
                            self.provider_requirement(local, &address, source);
                        }
                    }
                }
            }
        }
    }

    fn json_provider_requirements(&mut self, block: Node<'_>, text: &str) {
        for object in json_objects(block) {
            for (_, _, required) in json_pairs(object, text)
                .into_iter()
                .filter(|(key, _, _)| key == "required_providers")
            {
                for required in json_objects(required) {
                    for (local, _, value) in json_pairs(required, text) {
                        for (_, _, source) in json_pairs(value, text)
                            .into_iter()
                            .filter(|(key, _, _)| key == "source")
                        {
                            if let Some(address) = json_string(source, text) {
                                self.provider_requirement(&local, &address, source);
                            }
                        }
                    }
                }
            }
        }
    }

    fn provider_requirement(&mut self, local: &str, source: &str, span: Node<'_>) {
        if terraform_identifier(local) {
            // An explicit source never falls back to the implied
            // `hashicorp/<local>`; one Graphr cannot normalize keeps its own
            // lowercased identity and reports a gap.
            let provider = normalized_provider_source(source).unwrap_or_else(|| {
                self.provider_gap(None, span);
                source.to_ascii_lowercase()
            });
            self.graph.nodes[self.file_node]
                .keys
                .push(format!("{PROVIDER_SOURCE_KEY}{local}:{provider}"));
        }
    }

    fn mark_provider_configuration(&mut self) {
        self.graph
            .nodes
            .last_mut()
            .expect("just added provider")
            .keys
            .push(PROVIDER_CONFIG_KEY.to_owned());
    }

    fn resource_provider<'tree>(
        &mut self,
        resource: &str,
        resource_type: &str,
        explicit: Option<Node<'tree>>,
        read: impl FnOnce(Node<'tree>) -> Option<String>,
        declaration_line: u32,
    ) {
        if let Some(value) = explicit {
            if let Some(address) = read(value).filter(|address| provider_address(address).is_some())
            {
                self.provider_reference(resource, &address, line(value));
            } else {
                self.provider_gap(Some(resource), value);
            }
        } else if let Some(name) = resource_type
            .split('_')
            .next()
            .filter(|name| terraform_identifier(name))
        {
            self.provider_reference(resource, name, declaration_line);
        }
    }

    fn hcl_provider_mappings(
        &mut self,
        node: &str,
        address: &str,
        expression: Node<'_>,
        text: &str,
    ) {
        if let Some(pairs) = hcl_object_pairs(expression) {
            for (entry, key, value) in pairs {
                self.provider_mapping(
                    node,
                    address,
                    entry,
                    hcl_provider(key, text),
                    hcl_provider(value, text),
                );
            }
        } else {
            self.provider_gap(Some(node), expression);
        }
    }

    fn json_provider_mappings(
        &mut self,
        node: &str,
        address: &str,
        expression: Node<'_>,
        text: &str,
    ) {
        if expression.kind() == "object" {
            for (child, pair, value) in json_pairs(expression, text) {
                self.provider_mapping(node, address, pair, Some(child), json_string(value, text));
            }
        } else {
            self.provider_gap(Some(node), expression);
        }
    }

    fn provider_mapping(
        &mut self,
        node: &str,
        module: &str,
        span: Node<'_>,
        child: Option<String>,
        parent: Option<String>,
    ) {
        if let (Some(child), Some(parent)) = (child, parent)
            && provider_address(&child).is_some()
            && provider_address(&parent).is_some()
        {
            let address = format!("{module}.providers.{child}");
            let binding = self.declaration(span, address.clone(), None);
            self.reference(node, lookup_key(self.directory, &address), line(span));
            self.provider_reference(&binding, &parent, line(span));
        } else {
            self.provider_gap(Some(node), span);
        }
    }

    fn provider_reference(&mut self, source: &str, address: &str, line: u32) {
        self.reference(source, format!("{PROVIDER_REFERENCE_KEY}{address}"), line);
    }

    fn provider_gap(&mut self, source: Option<&str>, node: Node<'_>) {
        self.gap(
            source,
            line(node),
            end_line(node),
            GapReason::TerraformProviderUnmodeled,
            true,
        );
    }

    /// Graphr does not model `import` blocks, whose expressions Terraform
    /// evaluates for the target resource.
    fn import_gap(&mut self, block: Node<'_>) {
        self.gap(
            None,
            line(block),
            end_line(block),
            GapReason::TerraformImportUnmodeled,
            true,
        );
    }

    fn hcl_expression(
        &mut self,
        source: &str,
        node: Node<'_>,
        text: &str,
        fixed_line: Option<u32>,
        scope: &mut Vec<String>,
        depth: usize,
    ) {
        let line_for = |node| fixed_line.unwrap_or_else(|| line(node));
        if node.is_error() || node.is_missing() {
            return;
        }
        if depth == MAX_EXPRESSION_DEPTH {
            let start = line_for(node);
            self.gap(
                Some(source),
                start,
                start,
                GapReason::DynamicOrUnsupportedDispatch,
                true,
            );
            return;
        }
        let children = named_children(node);
        let original_scope = scope.len();
        let intro_kind = match node.kind() {
            "for_tuple_expr" | "for_object_expr" => Some("for_intro"),
            "template_for" => Some("template_for_start"),
            _ => None,
        };
        let mut dynamic_iterator = None;
        let block_kind = (node.kind() == "block")
            .then(|| children.first().map(|child| node_text(*child, text)))
            .flatten();
        if block_kind == Some("dynamic")
            && let Some(label) = children.get(1).and_then(|node| decode_label(*node, text))
        {
            dynamic_iterator = Some(label);
        }

        for child in children {
            if child.kind() == "variable_expr" {
                let root = node_text(child, text);
                if scope.iter().any(|binding| binding == root) {
                    continue;
                }
                let mut names = Vec::new();
                let siblings =
                    std::iter::successors(child.next_sibling(), |node| node.next_sibling());
                for next in siblings.filter(|node| !node.is_extra()) {
                    match next.kind() {
                        "get_attr" => {
                            if let Some(identifier) = next.named_child(0) {
                                names.push(node_text(identifier, text));
                            }
                        }
                        "index" => {}
                        "splat" => {
                            if let Some(splat) = next.named_child(0)
                                && let Some(attr) = named_children(splat)
                                    .into_iter()
                                    .find(|node| node.kind() == "get_attr")
                                && let Some(identifier) = attr.named_child(0)
                            {
                                names.push(node_text(identifier, text));
                            }
                            break;
                        }
                        _ => break,
                    }
                    if names.len() == 2 {
                        break;
                    }
                }
                self.traversal(source, root, &names, line_for(child));
            } else if intro_kind == Some(child.kind()) {
                for part in named_children(child)
                    .into_iter()
                    .filter(|part| part.kind() == "expression")
                {
                    self.hcl_expression(source, part, text, fixed_line, scope, depth + 1);
                }
                scope.extend(
                    named_children(child)
                        .into_iter()
                        .filter(|part| part.kind() == "identifier")
                        .map(|part| node_text(part, text).to_owned()),
                );
            } else if child.kind() == "body" && block_kind == Some("lifecycle") {
                for attr in named_children(child) {
                    if !matches!(hcl_attribute(attr, text), Some(("ignore_changes", _))) {
                        self.hcl_expression(source, attr, text, fixed_line, scope, depth + 1);
                    }
                }
            } else if child.kind() == "body"
                && let Some(default) = &dynamic_iterator
            {
                let attributes = named_children(child);
                let iterator = attributes
                    .iter()
                    .copied()
                    .filter_map(|attr| hcl_attribute(attr, text))
                    .find(|(name, _)| *name == "iterator")
                    .map_or(default.as_str(), |(_, value)| node_text(value, text));
                for attr in attributes {
                    match hcl_attribute(attr, text) {
                        Some(("iterator", _)) => {}
                        Some(("for_each", expression)) => self.hcl_expression(
                            source,
                            expression,
                            text,
                            fixed_line,
                            scope,
                            depth + 1,
                        ),
                        _ => {
                            scope.push(iterator.to_owned());
                            self.hcl_expression(source, attr, text, fixed_line, scope, depth + 1);
                            scope.pop();
                        }
                    }
                }
            } else {
                self.hcl_expression(source, child, text, fixed_line, scope, depth + 1);
            }
        }
        scope.truncate(original_scope);
    }

    /// Walks one argument of a JSON block body, where Terraform reserves
    /// `lifecycle` and `dynamic` as block types. Deeper objects can be map
    /// attributes, so `json_expression` gives those keys no meaning.
    fn json_argument(
        &mut self,
        source: &str,
        name: &str,
        value: Node<'_>,
        scope: &mut Vec<String>,
        parser: &mut Option<Parser>,
    ) -> Result<(), String> {
        let text = &self.source.text;
        match name {
            "lifecycle" => {
                for (name, _, value) in json_objects(value)
                    .into_iter()
                    .flat_map(|body| json_pairs(body, text))
                {
                    // Terraform reads these strings as bare references.
                    let bare = name == "replace_triggered_by";
                    self.json_expression(source, value, bare, scope, parser)?;
                }
            }
            // Recursion stays within the serde_json nesting limit checked in `add_file`.
            "dynamic" => {
                for (label, _, blocks) in json_objects(value)
                    .into_iter()
                    .flat_map(|labels| json_pairs(labels, text))
                {
                    for block in json_objects(blocks) {
                        let arguments = json_pairs(block, text);
                        let iterator = arguments
                            .iter()
                            .find(|(name, _, _)| name == "iterator")
                            .and_then(|(_, _, iterator)| json_string(*iterator, text))
                            .unwrap_or_else(|| label.clone());
                        for (name, _, value) in arguments {
                            match name.as_str() {
                                "iterator" => {}
                                // As in HCL, `for_each` cannot use its own iterator.
                                "for_each" => {
                                    self.json_expression(source, value, false, scope, parser)?;
                                }
                                "content" => {
                                    scope.push(iterator.clone());
                                    for (name, _, value) in json_objects(value)
                                        .into_iter()
                                        .flat_map(|body| json_pairs(body, text))
                                    {
                                        self.json_argument(source, &name, value, scope, parser)?;
                                    }
                                    scope.pop();
                                }
                                _ => {
                                    scope.push(iterator.clone());
                                    self.json_expression(source, value, false, scope, parser)?;
                                    scope.pop();
                                }
                            }
                        }
                    }
                }
            }
            _ => self.json_expression(source, value, name == "depends_on", scope, parser)?,
        }
        Ok(())
    }

    fn json_expression(
        &mut self,
        source: &str,
        root: Node<'_>,
        bare_reference: bool,
        scope: &mut Vec<String>,
        parser: &mut Option<Parser>,
    ) -> Result<(), String> {
        let text = &self.source.text;
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            match node.kind() {
                // Terraform reads object keys in expressions as string templates.
                "pair" => pending.extend(
                    ["value", "key"]
                        .map(|field| node.child_by_field_name(field))
                        .into_iter()
                        .flatten(),
                ),
                "array" | "object" => pending.extend(named_children(node).into_iter().rev()),
                "string" => {
                    let Some(value) = json_string(node, text) else {
                        continue;
                    };
                    if !bare_reference && !value.contains("${") && !value.contains("%{") {
                        continue;
                    }
                    self.string_expression(source, &value, node, bare_reference, scope, parser)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Parses a string Terraform evaluates as a bare reference or a template,
    /// attributing its references to the line of `span`.
    fn string_expression(
        &mut self,
        source: &str,
        value: &str,
        span: Node<'_>,
        bare_reference: bool,
        scope: &mut Vec<String>,
        parser: &mut Option<Parser>,
    ) -> Result<(), String> {
        let expression = if bare_reference {
            format!("value = {value}\n")
        } else {
            let mut marker = "GRAPHR_TEMPLATE".to_owned();
            while value.lines().any(|line| line.trim() == marker) {
                marker.push('_');
            }
            format!("value = <<{marker}\n{value}\n{marker}\n")
        };
        if let Some(tree) = parse(parser, || tree_sitter_hcl::LANGUAGE.into(), &expression)?
            && !tree.root_node().has_error()
            && let Some(body) = tree.root_node().named_child(0)
            && body.named_child_count() == 1
            && let Some((_, value)) = body
                .named_child(0)
                .and_then(|attr| hcl_attribute(attr, &expression))
        {
            self.hcl_expression(source, value, &expression, Some(line(span)), scope, 0);
        } else {
            self.gap(
                Some(source),
                line(span),
                end_line(span),
                GapReason::ParserError,
                true,
            );
        }
        Ok(())
    }

    fn traversal(&mut self, source: &str, root: &str, attributes: &[&str], line: u32) {
        let address = match (root, attributes) {
            ("data" | "ephemeral", [kind, name, ..]) => format!("{root}.{kind}.{name}"),
            ("module", [name, output, ..]) => format!("module.{name}.{output}"),
            ("var" | "local" | "module", [name, ..]) => format!("{root}.{name}"),
            (
                "path" | "terraform" | "count" | "each" | "self" | "data" | "ephemeral" | "var"
                | "local" | "module",
                _,
            ) => return,
            (_, [name, ..]) => format!("{root}.{name}"),
            _ => return,
        };
        self.reference(source, lookup_key(self.directory, &address), line);
    }

    /// Terraform still accepts deprecated quoted references in `depends_on`.
    fn hcl_depends_on(
        &mut self,
        source: &str,
        expression: Node<'_>,
        text: &str,
        parser: &mut Option<Parser>,
    ) -> Result<(), String> {
        let elements = hcl_collection(expression)
            .filter(|node| node.kind() == "tuple")
            .map_or_else(|| vec![expression], named_children);
        for element in elements {
            if let Some(reference) = hcl_string(element, text) {
                self.string_expression(source, &reference, element, true, &mut Vec::new(), parser)?;
            } else {
                self.hcl_expression(source, element, text, None, &mut Vec::new(), 0);
            }
        }
        Ok(())
    }

    fn reference(&mut self, source: &str, key: String, line: u32) {
        self.graph
            .refs
            .push(dependency(source.to_owned(), key, line));
    }

    fn gap(
        &mut self,
        source: Option<&str>,
        start: u32,
        end: u32,
        reason: GapReason,
        relation_site: bool,
    ) {
        self.graph.gaps.push(GapInput {
            file_key: Some(self.source.path.clone()),
            source_key: source.map(str::to_owned),
            run_key: None,
            path: Some(self.source.path.clone()),
            line_start: Some(start),
            line_end: Some(end),
            category: match reason {
                GapReason::ExternalDependency => GapCategory::Boundary,
                GapReason::DynamicOrUnsupportedDispatch
                | GapReason::TerraformProviderUnmodeled
                | GapReason::TerraformImportUnmodeled => GapCategory::Relation,
                GapReason::TerraformOverride => GapCategory::Language,
                _ => GapCategory::Parse,
            },
            reason,
            target_hint: None,
            occurrences: 1,
            relation_site,
        });
    }
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|node| !node.is_extra())
        .collect()
}

fn hcl_attribute<'tree, 'text>(
    node: Node<'tree>,
    text: &'text str,
) -> Option<(&'text str, Node<'tree>)> {
    if node.kind() != "attribute" {
        return None;
    }
    let children = named_children(node);
    let [name, value] = children.as_slice() else {
        return None;
    };
    Some((node_text(*name, text), *value))
}

fn json_pairs<'a>(node: Node<'a>, text: &str) -> Vec<(String, Node<'a>, Node<'a>)> {
    if node.kind() != "object" {
        return Vec::new();
    }
    named_children(node)
        .into_iter()
        .filter_map(|pair| {
            let key = json_string(pair.child_by_field_name("key")?, text)?;
            Some((key, pair, pair.child_by_field_name("value")?))
        })
        .collect()
}

fn json_objects(node: Node<'_>) -> Vec<Node<'_>> {
    match node.kind() {
        "object" => vec![node],
        "array" => named_children(node)
            .into_iter()
            .filter(|entry| entry.kind() == "object")
            .collect(),
        _ => Vec::new(),
    }
}

/// Unwraps single-element expression wrappers down to a tuple, object, or term.
fn hcl_collection(mut node: Node<'_>) -> Option<Node<'_>> {
    while matches!(node.kind(), "expression" | "collection_value") {
        let children = named_children(node);
        let [child] = children.as_slice() else {
            return None;
        };
        node = *child;
    }
    Some(node)
}

fn hcl_object_pairs(node: Node<'_>) -> Option<Vec<(Node<'_>, Node<'_>, Node<'_>)>> {
    let node = hcl_collection(node)?;
    (node.kind() == "object").then(|| {
        named_children(node)
            .into_iter()
            .filter_map(|entry| {
                Some((
                    entry,
                    entry.child_by_field_name("key")?,
                    entry.child_by_field_name("val")?,
                ))
            })
            .collect()
    })
}

fn hcl_provider(node: Node<'_>, text: &str) -> Option<String> {
    let children = named_children(node);
    match children.as_slice() {
        [name] if name.kind() == "variable_expr" => Some(node_text(*name, text).to_owned()),
        [name, alias] if name.kind() == "variable_expr" && alias.kind() == "get_attr" => {
            Some(format!(
                "{}.{}",
                node_text(*name, text),
                node_text(alias.named_child(0)?, text)
            ))
        }
        _ => None,
    }
}

fn json_string(node: Node<'_>, text: &str) -> Option<String> {
    (node.kind() == "string")
        .then(|| serde_json::from_str(node_text(node, text)).ok())
        .flatten()
}

fn decode_label(node: Node<'_>, text: &str) -> Option<String> {
    match node.kind() {
        "identifier" => Some(node_text(node, text).to_owned()),
        "string_lit" => serde_json::from_str(node_text(node, text)).ok(),
        _ => None,
    }
}

fn hcl_labels(block: Node<'_>, text: &str) -> Option<Vec<String>> {
    named_children(block)
        .into_iter()
        .skip(1)
        .take_while(|child| child.kind() != "block_start")
        .map(|child| decode_label(child, text))
        .collect()
}

fn hcl_string(node: Node<'_>, text: &str) -> Option<String> {
    let literal = node.named_child(0)?;
    (node.named_child_count() == 1 && literal.kind() == "literal_value")
        .then(|| decode_label(literal.named_child(0)?, text))
        .flatten()
}

fn declaration_address(kind: &str, labels: &[String], alias: Option<&str>) -> Option<String> {
    let address = match (kind, labels) {
        ("provider", [name]) if terraform_identifier(name) => alias.map_or_else(
            || format!("provider.{name}"),
            |alias| format!("provider.{name}.{alias}"),
        ),
        ("resource", [kind, name]) => format!("{kind}.{name}"),
        ("data" | "ephemeral", [type_name, name]) => format!("{kind}.{type_name}.{name}"),
        ("variable", [name]) => format!("var.{name}"),
        ("module" | "output" | "check", [name]) => format!("{kind}.{name}"),
        _ => return None,
    };
    (!address.chars().any(char::is_control)).then_some(address)
}

fn resource_type<'a>(kind: &str, labels: &'a [String]) -> Option<&'a str> {
    match (kind, labels) {
        ("resource" | "data" | "ephemeral", [name, _]) => Some(name),
        _ => None,
    }
}

fn terraform_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|ch| ch.is_alphabetic() || ch == '_')
        && characters.all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-')
}

fn provider_address(address: &str) -> Option<(&str, &str)> {
    let (name, alias) = address
        .split_once('.')
        .map_or((address, None), |(name, alias)| (name, Some(alias)));
    (terraform_identifier(name) && alias.is_none_or(terraform_identifier))
        .then_some((name, alias.unwrap_or("")))
}

fn normalized_provider_source(address: &str) -> Option<String> {
    let parts = address.split('/').collect::<Vec<_>>();
    let source_part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_')
    };
    match parts.as_slice() {
        [namespace, name] if source_part(namespace) && source_part(name) => {
            Some(format!("registry.terraform.io/{address}").to_ascii_lowercase())
        }
        [host, namespace, name] if source_part(namespace) && source_part(name) => {
            // As svchost does: an optional decimal port, without the default 443.
            let (name_part, port) = host
                .split_once(':')
                .map_or((*host, None), |(host, port)| (host, Some(port)));
            let port = match port.map(str::parse::<u16>) {
                None | Some(Ok(443)) => String::new(),
                Some(Ok(port)) => format!(":{port}"),
                Some(Err(_)) => return None,
            };
            (!name_part.is_empty()
                && name_part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.'))
            .then(|| format!("{name_part}{port}/{namespace}/{name}").to_ascii_lowercase())
        }
        _ => None,
    }
}

fn literal_hcl_argument(kind: &str, name: &str) -> bool {
    matches!(
        (kind, name),
        ("variable", "type" | "default")
            | ("module", "version" | "providers")
            | ("provider", "alias" | "version")
            | ("resource" | "data" | "ephemeral", "provider")
    )
}

pub(crate) fn lookup_key(directory: &str, address: &str) -> String {
    format!("terraform:{}:{directory}:{address}", directory.len())
}

fn lookup_address(key: &str) -> Option<(&str, &str)> {
    let (size, remainder) = key.strip_prefix("terraform:")?.split_once(':')?;
    let (directory, address) = remainder.split_at_checked(size.parse().ok()?)?;
    Some((directory, address.strip_prefix(':')?))
}

/// Renders a lookup key as `directory:address`, or as the address at the
/// repository root.
pub(crate) fn display_key(key: &str) -> Option<String> {
    let (directory, address) = lookup_address(key)?;
    Some(if directory.is_empty() {
        address.to_owned()
    } else {
        format!("{directory}:{address}")
    })
}

fn module_directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

fn module_source(node: &NodeInput) -> Option<&str> {
    node.keys
        .iter()
        .find_map(|key| key.strip_prefix(MODULE_SOURCE_KEY))
}

fn module_call(address: &str) -> bool {
    address
        .strip_prefix("module.")
        .is_some_and(|name| !name.contains('.'))
}

fn module_output_call(address: &str) -> Option<&str> {
    let (call, _) = address.rsplit_once('.')?;
    module_call(call).then_some(call)
}

fn provider_mapping_address(address: &str) -> Option<(&str, &str)> {
    let (call, selection) = address.split_once(".providers.")?;
    module_call(call).then_some((call, selection))
}

fn dependency(source_key: String, key: String, line: u32) -> RefInput {
    RefInput {
        source_key,
        kind: RefKind::DependsOn,
        line,
        keys: vec![key],
        alias_key: None,
        resolved_target_key: None,
        resolution: ResolutionState::Pending,
    }
}

#[derive(Default)]
struct Module<'a> {
    declarations: Vec<&'a NodeInput>,
    symbols: HashSet<&'a str>,
    calls: BTreeMap<&'a str, Option<&'a NodeInput>>,
    records: usize,
    bytes: usize,
    provider_requirements: HashMap<String, String>,
    provider_configurations: HashMap<ProviderSlot, Option<&'a str>>,
    provider_mappings: HashMap<&'a str, Vec<(&'a str, &'a str)>>,
}

#[derive(Eq, Hash, PartialEq)]
struct ProviderSlot {
    source: String,
    alias: String,
}

impl Module<'_> {
    fn provider_slot(&self, address: &str) -> Option<ProviderSlot> {
        let (local, alias) = provider_address(address)?;
        let source = self
            .provider_requirements
            .get(local)
            .cloned()
            .unwrap_or_else(|| {
                if local == "terraform" {
                    "terraform.io/builtin/terraform".to_owned()
                } else {
                    format!(
                        "registry.terraform.io/hashicorp/{}",
                        local.to_ascii_lowercase()
                    )
                }
            });
        Some(ProviderSlot {
            source,
            alias: alias.to_owned(),
        })
    }
}

fn remember_provider<'a>(
    providers: &mut HashMap<ProviderSlot, Option<&'a str>>,
    slot: ProviderSlot,
    address: &'a str,
) {
    providers
        .entry(slot)
        .and_modify(|current| {
            if *current != Some(address) {
                *current = None;
            }
        })
        .or_insert(Some(address));
}

fn child_provider_mappings<'a>(
    caller: &Module<'a>,
    child: &Module<'_>,
    call: &str,
) -> HashMap<ProviderSlot, Option<&'a str>> {
    let mut mappings = HashMap::new();
    for &(selection, address) in caller.provider_mappings.get(call).into_iter().flatten() {
        if let Some(slot) = child.provider_slot(selection) {
            remember_provider(&mut mappings, slot, address);
        }
    }
    mappings
}

struct Instance<'a> {
    root: &'a str,
    directory: &'a str,
    prefix: String,
    caller: Option<(usize, &'a str)>,
    depth: usize,
    children: BTreeMap<&'a str, usize>,
    controls: Vec<String>,
    provider_mappings: HashMap<ProviderSlot, Option<&'a str>>,
}

impl<'a> Instance<'a> {
    fn root(directory: &'a str) -> Self {
        Self {
            root: directory,
            directory,
            prefix: String::new(),
            caller: None,
            depth: 0,
            children: BTreeMap::new(),
            controls: Vec::new(),
            provider_mappings: HashMap::new(),
        }
    }

    fn address(&self, symbol: &str) -> String {
        if self.prefix.is_empty() {
            symbol.to_owned()
        } else {
            format!("{}.{symbol}", self.prefix)
        }
    }

    fn lookup(&self, symbol: &str) -> String {
        lookup_key(self.root, &self.address(symbol))
    }
}

fn provider_target(
    modules: &BTreeMap<&str, Module<'_>>,
    instances: &[Instance<'_>],
    mut index: usize,
    slot: &ProviderSlot,
) -> Option<String> {
    loop {
        let instance = &instances[index];
        if let Some(address) = instance.provider_mappings.get(slot) {
            let (caller, _) = instance.caller?;
            return address.map(|address| instances[caller].lookup(address));
        }
        if let Some(address) = modules[instance.directory]
            .provider_configurations
            .get(slot)
        {
            return address.map(|address| instance.lookup(address));
        }
        // Terraform inherits default configurations by source address. An
        // alias must be mapped explicitly at each boundary where it is used.
        if !slot.alias.is_empty() {
            return None;
        }
        index = instance.caller?.0;
    }
}

/// Expand local module calls before reference resolution so each call receives
/// its own declarations and its own bindings to the caller's expressions.
pub(crate) fn project_modules(graph: &mut Graph, cancelled: &AtomicBool) -> Result<(), String> {
    let (declarations, other_nodes): (Vec<_>, Vec<_>) = std::mem::take(&mut graph.nodes)
        .into_iter()
        .partition(|node| node.kind == NodeKind::Terraform);
    graph.nodes = other_nodes;
    let (terraform_refs, other_refs): (Vec<_>, Vec<_>) = std::mem::take(&mut graph.refs)
        .into_iter()
        .partition(|reference| reference.kind == RefKind::DependsOn);
    graph.refs = other_refs;
    let mut modules = BTreeMap::<&str, Module<'_>>::new();
    let mut sites = HashMap::new();
    for file in graph
        .files
        .iter()
        .filter(|file| file.language == SourceLanguage::Terraform && file.replace)
    {
        let module = modules.entry(module_directory(&file.path)).or_default();
        module.bytes = module
            .bytes
            .saturating_add(usize::try_from(file.byte_size).unwrap_or(usize::MAX));
        sites.insert(file.path.clone(), 0_usize);
    }
    for file_node in graph
        .nodes
        .iter_mut()
        .filter(|node| node.kind == NodeKind::File && sites.contains_key(&node.file_key))
    {
        let module = modules
            .get_mut(module_directory(&file_node.file_key))
            .expect("Terraform file inventoried");
        file_node.keys.retain(|key| {
            if let Some((local, source)) = key
                .strip_prefix(PROVIDER_SOURCE_KEY)
                .and_then(|key| key.split_once(':'))
            {
                module
                    .provider_requirements
                    .insert(local.to_owned(), source.to_owned());
                false
            } else {
                true
            }
        });
    }
    let mut refs_by_node = HashMap::<_, Vec<_>>::new();
    for reference in &terraform_refs {
        check_projection_cancelled(cancelled)?;
        refs_by_node
            .entry(reference.source_key.as_str())
            .or_default()
            .push(reference);
    }
    for node in &declarations {
        check_projection_cancelled(cancelled)?;
        let module = modules
            .get_mut(module_directory(&node.file_key))
            .ok_or("Terraform declaration file is missing")?;
        module.declarations.push(node);
        module.symbols.insert(&node.name);
        // Each copy has a declaration, an optional input binding and a
        // possible edge from the parent call. Controls vary per call.
        module.records += 3 + refs_by_node.get(node.key.as_str()).map_or(0, Vec::len);
        if module_call(&node.name) {
            module
                .calls
                .entry(&node.name)
                .and_modify(|call| *call = None)
                .or_insert(Some(node));
        }
        if node.keys.iter().any(|key| key == PROVIDER_CONFIG_KEY)
            && let Some(selection) = node
                .name
                .strip_prefix("provider.")
                .and_then(|name| module.provider_slot(name))
        {
            remember_provider(&mut module.provider_configurations, selection, &node.name);
        }
        if let Some((call, selection)) = provider_mapping_address(&node.name) {
            module
                .provider_mappings
                .entry(call)
                .or_default()
                .push((selection, &node.name));
        }
    }

    let (instances, boundaries) = module_instances(&modules, cancelled)?;
    for gap in &mut graph.gaps {
        if let Some(count) = gap.file_key.as_ref().and_then(|path| sites.get_mut(path)) {
            // A parse gap is shared by all calls at the same source location.
            gap.source_key = None;
            if gap.relation_site {
                *count += gap.occurrences as usize;
            }
        }
    }
    for (instance_id, instance) in instances.iter().enumerate() {
        let parent = instance
            .caller
            .map(|(parent, call)| (&instances[parent], call));
        for node in &modules[instance.directory].declarations {
            check_projection_cancelled(cancelled)?;
            let address = instance.address(&node.name);
            let key = if instance.prefix.is_empty() {
                node.key.clone()
            } else {
                format!("{}:{}", lookup_key(instance.root, &address), node.key)
            };
            let mut keys = vec![lookup_key(instance.root, &address)];
            if !instance.prefix.is_empty()
                && let Some(output) = node.name.strip_prefix("output.")
            {
                keys.push(lookup_key(
                    instance.root,
                    &format!("{}.{output}", instance.prefix),
                ));
            }
            let refs_before = graph.refs.len();
            let provider_gaps_before = graph.gaps.len();
            for reference in refs_by_node.get(node.key.as_str()).into_iter().flatten() {
                let raw = reference
                    .keys
                    .first()
                    .ok_or("Terraform reference address is missing")?;
                if let Some(address) = raw.strip_prefix(PROVIDER_REFERENCE_KEY) {
                    let provider = modules[instance.directory]
                        .provider_slot(address)
                        .ok_or("invalid provider reference")?;
                    let incompatible_mapping = provider_mapping_address(&node.name)
                        .and_then(|(call, child_address)| {
                            let &child = instance.children.get(call)?;
                            modules[instances[child].directory].provider_slot(child_address)
                        })
                        .is_some_and(|child| child.source != provider.source);
                    if !incompatible_mapping
                        && let Some(target) =
                            provider_target(&modules, &instances, instance_id, &provider)
                    {
                        graph
                            .refs
                            .push(dependency(key.clone(), target, reference.line));
                    } else if incompatible_mapping || !provider.alias.is_empty() {
                        let reason = if incompatible_mapping {
                            GapReason::TerraformProviderUnmodeled
                        } else {
                            GapReason::UnresolvedDependency
                        };
                        let mut gap =
                            module_gap(node, &key, reason, Some(&format!("provider.{address}")));
                        gap.category = GapCategory::Relation;
                        gap.line_start = Some(reference.line);
                        gap.line_end = Some(reference.line);
                        graph.gaps.push(gap);
                    }
                    continue;
                }
                let (_, target) =
                    lookup_address(raw).ok_or("Terraform reference address is missing")?;
                let target = module_output_call(target)
                    .filter(|call| {
                        modules[instance.directory].calls.contains_key(call)
                            && !instance.children.contains_key(call)
                    })
                    .unwrap_or(target);
                graph.refs.push(dependency(
                    key.clone(),
                    instance.lookup(target),
                    reference.line,
                ));
            }
            if let Some((parent, call)) = parent
                && let Some(name) = node.name.strip_prefix("var.")
            {
                let binding = format!("{call}.input.{name}");
                if modules[parent.directory].symbols.contains(binding.as_str()) {
                    graph.refs.push(dependency(
                        key.clone(),
                        parent.lookup(&binding),
                        node.line_start,
                    ));
                }
            }
            graph.refs.extend(
                instance
                    .controls
                    .iter()
                    .map(|control| dependency(key.clone(), control.clone(), node.line_start)),
            );
            if let Some(&child) = instance.children.get(node.name.as_str()) {
                let child = &instances[child];
                for declaration in &modules[child.directory].declarations {
                    check_projection_cancelled(cancelled)?;
                    if !declaration.name.starts_with("var.")
                        && !declaration.name.starts_with("local.")
                    {
                        graph.refs.push(dependency(
                            key.clone(),
                            child.lookup(&declaration.name),
                            node.line_start,
                        ));
                    }
                }
            }
            let count = sites
                .get_mut(&node.file_key)
                .ok_or("Terraform declaration file is missing")?;
            *count += (graph.refs.len() - refs_before) + (graph.gaps.len() - provider_gaps_before);
            if let Some(&reason) = boundaries.get(&(instance_id, node.name.as_str())) {
                graph
                    .gaps
                    .push(module_gap(node, &key, reason, module_source(node)));
                *count += 1;
            }
            if let Some((call, binding)) = node.name.split_once(".input.")
                && let Some(child) = instance.children.get(call).map(|&child| &instances[child])
            {
                let variable = format!("var.{binding}");
                if !modules[child.directory].symbols.contains(variable.as_str()) {
                    graph.gaps.push(module_gap(
                        node,
                        &key,
                        GapReason::UnresolvedDependency,
                        Some(&child.lookup(&variable)),
                    ));
                    *count += 1;
                }
            }
            graph.nodes.push(NodeInput {
                qualified_name: key.clone(),
                key,
                file_key: node.file_key.clone(),
                kind: node.kind,
                name: if !instance.prefix.is_empty() && !instance.root.is_empty() {
                    format!("{}::{address}", instance.root)
                } else {
                    address
                },
                parent_key: node.parent_key.clone(),
                owner_key: None,
                line_start: node.line_start,
                line_end: node.line_end,
                signature: String::new(),
                keys,
            });
        }
    }
    for file in &mut graph.files {
        if let Some(count) = sites.get(&file.path) {
            file.observed_relation_sites =
                u32::try_from(*count).map_err(|_| "too many Terraform dependency sites")?;
        }
    }
    Ok(())
}

type ModuleBoundaries<'a> = HashMap<(usize, &'a str), GapReason>;

fn module_instances<'a>(
    modules: &BTreeMap<&'a str, Module<'a>>,
    cancelled: &AtomicBool,
) -> Result<(Vec<Instance<'a>>, ModuleBoundaries<'a>), String> {
    let called = modules
        .values()
        .flat_map(|module| module.calls.values())
        .filter_map(|call| module_source((*call)?))
        .collect::<HashSet<_>>();
    let mut instances = modules
        .keys()
        .copied()
        .filter(|directory| !called.contains(directory))
        .map(Instance::root)
        .collect::<Vec<_>>();
    let mut covered = instances
        .iter()
        .map(|instance| instance.directory)
        .collect::<HashSet<_>>();
    let mut unvisited = modules.keys().copied();
    let mut boundaries = HashMap::new();
    let mut records_left = MAX_CHILD_RECORDS;
    let mut bytes_left = MAX_CHILD_BYTES;
    let mut index = 0;
    loop {
        if index == instances.len() {
            // Cycles and bounded expansions can leave directories with no caller coverage.
            let Some(directory) = unvisited.find(|directory| !covered.contains(directory)) else {
                break;
            };
            instances.push(Instance::root(directory));
            covered.insert(directory);
        }
        for (&address, node) in &modules[instances[index].directory].calls {
            check_projection_cancelled(cancelled)?;
            let Some(directory) = node.and_then(module_source) else {
                continue;
            };
            let parent = &instances[index];
            let mut ancestors = std::iter::successors(Some(index), |&id| {
                instances[id].caller.map(|(caller, _)| caller)
            });
            let reason = if ancestors.any(|id| instances[id].directory == directory) {
                Some(GapReason::TerraformModuleCycle)
            } else if let Some(module) = modules.get(directory) {
                let prefix = parent.address(address);
                let mut controls = parent.controls.clone();
                for argument in MODULE_CONTROL_ARGUMENTS {
                    let control = format!("{address}.configuration.{argument}");
                    if modules[parent.directory].symbols.contains(control.as_str()) {
                        controls.push(parent.lookup(&control));
                    }
                }
                let control_records = controls.len().saturating_mul(module.declarations.len());
                let records = module.records.saturating_add(control_records).max(1);
                let bytes = module.bytes.saturating_add(
                    records.saturating_mul(prefix.len().saturating_add(parent.root.len())),
                );
                if parent.depth == MAX_MODULE_DEPTH || records > records_left || bytes > bytes_left
                {
                    Some(GapReason::TerraformModuleLimit)
                } else {
                    let child = Instance {
                        root: parent.root,
                        directory,
                        prefix,
                        caller: Some((index, address)),
                        depth: parent.depth + 1,
                        children: BTreeMap::new(),
                        controls,
                        provider_mappings: child_provider_mappings(
                            &modules[parent.directory],
                            module,
                            address,
                        ),
                    };
                    let child_id = instances.len();
                    instances.push(child);
                    instances[index].children.insert(address, child_id);
                    covered.insert(directory);
                    records_left -= records;
                    bytes_left -= bytes;
                    None
                }
            } else {
                Some(GapReason::ExternalDependency)
            };
            if let Some(reason) = reason {
                boundaries.insert((index, address), reason);
            }
        }
        index += 1;
    }
    Ok((instances, boundaries))
}

fn module_gap(node: &NodeInput, source: &str, reason: GapReason, target: Option<&str>) -> GapInput {
    GapInput {
        file_key: Some(node.file_key.clone()),
        source_key: Some(source.to_owned()),
        run_key: None,
        path: Some(node.file_key.clone()),
        line_start: Some(node.line_start),
        line_end: Some(node.line_end),
        category: GapCategory::Boundary,
        reason,
        target_hint: target.map(str::to_owned),
        occurrences: 1,
        relation_site: true,
    }
}

fn check_projection_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("index cancelled".into())
    } else {
        Ok(())
    }
}

fn local_directory(directory: &str, path: &str) -> Option<String> {
    if (!path.starts_with("./") && !path.starts_with("../"))
        || path.chars().any(|ch| ch.is_control() || ch == '\\')
    {
        return None;
    }
    let mut parts: Vec<_> = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(name) => parts.push(name.to_str()?),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.contains(&".git") || parts.contains(&".terraform") {
        return None;
    }
    Some(parts.join("/"))
}

fn is_override(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(name, "override.tf" | "override.tf.json")
        || name.ends_with("_override.tf")
        || name.ends_with("_override.tf.json")
}

fn node_text<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    text.get(node.byte_range()).unwrap_or_default()
}

fn line(node: Node<'_>) -> u32 {
    (node.start_position().row + 1) as u32
}

fn end_line(node: Node<'_>) -> u32 {
    let end = node.end_position();
    ((end.row + usize::from(end.column > 0)) as u32).max(line(node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Language;
    use crate::store::FileInput;

    fn parse_source(source: &Source) -> Graph {
        parse_sources(std::slice::from_ref(source))
    }

    fn parse_sources(sources: &[Source]) -> Graph {
        let mut graph = Graph {
            files: sources
                .iter()
                .map(|source| FileInput {
                    path: source.path.clone(),
                    language: Language::Terraform,
                    git_oid: None,
                    content_hash: *blake3::hash(source.text.as_bytes()).as_bytes(),
                    parse_context: String::new(),
                    byte_size: source.text.len() as u64,
                    replace: true,
                    observed_relation_sites: 0,
                })
                .collect(),
            ..Graph::default()
        };
        let mut parsers = TerraformParsers::default();
        for source in sources {
            add_file(&mut graph, source, &mut parsers).unwrap();
        }
        graph
    }

    #[test]
    fn expression_dependencies_distinguish_iterators_from_module_addresses() {
        let source = Source {
            path: "infra/main.tf".into(),
            text: r#"
locals {
  names = [for local in var.groups : local.name if var.enabled]
  id    = aws_vpc.main[0].id
}

resource "example" "dynamic" {
  lifecycle {
    ignore_changes = [tags.Name]
  }

  dynamic "entry" {
    for_each = var.groups
    iterator = group
    content {
      value = "${group.value}-${local.id}"
    }
  }
}
"#
            .into(),
        };
        let mut graph = parse_source(&source);
        project_modules(&mut graph, &AtomicBool::new(false)).unwrap();
        assert!(graph.gaps.is_empty());
        let references = graph
            .refs
            .iter()
            .flat_map(|reference| &reference.keys)
            .map(|key| lookup_address(key).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            references,
            [
                ("infra", "var.groups"),
                ("infra", "var.enabled"),
                ("infra", "aws_vpc.main"),
                ("infra", "var.groups"),
                ("infra", "local.id"),
            ]
        );
    }

    #[test]
    fn recursive_module_keeps_its_declarations_and_reports_the_cycle() {
        let mut graph = parse_source(&Source {
            path: "infra/main.tf".into(),
            text: "module \"self\" { source = \"./.\" }\n".into(),
        });
        project_modules(&mut graph, &AtomicBool::new(false)).unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph.nodes.iter().any(|node| node.name == "module.self"));
        assert_eq!(graph.gaps.len(), 1);
        assert_eq!(graph.gaps[0].reason, GapReason::TerraformModuleCycle);
        assert_eq!(graph.files[0].observed_relation_sites, 1);
    }

    /// Source declaration and target address of every dependency, sorted.
    fn dependencies(graph: &Graph) -> Vec<(&str, &str)> {
        let mut dependencies = graph
            .refs
            .iter()
            .map(|reference| {
                let source = graph
                    .nodes
                    .iter()
                    .find(|node| node.key == reference.source_key)
                    .unwrap();
                (
                    source.name.as_str(),
                    lookup_address(&reference.keys[0]).unwrap().1,
                )
            })
            .collect::<Vec<_>>();
        dependencies.sort_unstable();
        dependencies
    }

    #[test]
    fn check_scoped_data_sources_are_module_declarations() {
        for (path, text) in [
            (
                "infra/main.tf",
                r#"
check "health" {
  data "http" "probe" {
    url = var.url
  }
  assert {
    condition     = var.enabled
    error_message = "down"
  }
}
"#,
            ),
            (
                "infra/main.tf.json",
                r#"{"check": {"health": {
  "data": {"http": {"probe": {"url": "${var.url}"}}},
  "assert": [{"condition": "${var.enabled}", "error_message": "down"}]
}}}"#,
            ),
        ] {
            let mut graph = parse_source(&Source {
                path: path.into(),
                text: text.into(),
            });
            project_modules(&mut graph, &AtomicBool::new(false)).unwrap();
            assert!(graph.gaps.is_empty(), "{path}");
            assert_eq!(
                dependencies(&graph),
                [
                    // Terraform links a check to its nested data source.
                    ("check.health", "data.http.probe"),
                    ("check.health", "var.enabled"),
                    ("data.http.probe", "var.url"),
                ],
                "{path}"
            );
        }
    }

    #[test]
    fn json_lifecycle_and_dynamic_blocks_follow_terraform_references() {
        let mut graph = parse_source(&Source {
            path: "infra/main.tf.json".into(),
            text: r#"{"resource": {
  "aws_instance": {"x": {"ami": "abc"}},
  "aws_security_group": {"sg": {
    "tags": {
      "ignore_changes": "${var.team}",
      "replace_triggered_by": "${var.owner}",
      "dynamic": {"var": {"key": "${var.env}"}}
    },
    "dynamic": {"ingress": [
      {"for_each": "${var.ports}", "content": {"from_port": "${ingress.value}"}},
      {"for_each": "${var.ports}", "iterator": "rule", "content": {
        "to_port": "${rule.value}",
        "dynamic": {"cidr": {"for_each": "${rule.value.cidrs}", "content": {"block": "${cidr.value}"}}}
      }},
      {"for_each": "${ingress.extra}", "labels": ["${ingress.key}"], "content": {}}
    ]},
    "lifecycle": {
      "ignore_changes": ["tags"],
      "replace_triggered_by": ["aws_instance.x.id"],
      "precondition": [{"condition": "${var.ok}", "error_message": "x"}]
    }
  }}
}}"#
            .into(),
        });
        project_modules(&mut graph, &AtomicBool::new(false)).unwrap();
        assert!(graph.gaps.is_empty());
        assert_eq!(
            dependencies(&graph),
            [
                ("aws_security_group.sg", "aws_instance.x"),
                // `for_each` cannot use the iterator it defines.
                ("aws_security_group.sg", "ingress.extra"),
                ("aws_security_group.sg", "var.env"),
                ("aws_security_group.sg", "var.ok"),
                ("aws_security_group.sg", "var.owner"),
                ("aws_security_group.sg", "var.ports"),
                ("aws_security_group.sg", "var.ports"),
                ("aws_security_group.sg", "var.team"),
            ]
        );
    }

    #[test]
    fn json_locals_blocks_and_templated_object_keys_keep_references() {
        let mut graph = parse_source(&Source {
            path: "infra/main.tf.json".into(),
            text: r#"{
  "locals": [
    {"a": "${var.input}", "map": {"${var.key}": "value"}},
    {"b": "${local.a}"}
  ],
  "output": {"result": {"value": "${local.b}"}}
}"#
            .into(),
        });
        project_modules(&mut graph, &AtomicBool::new(false)).unwrap();
        assert!(graph.gaps.is_empty());
        assert_eq!(
            dependencies(&graph),
            [
                ("local.a", "var.input"),
                ("local.b", "local.a"),
                ("local.map", "var.key"),
                ("output.result", "local.b"),
            ]
        );
    }

    #[test]
    fn quoted_hcl_depends_on_follows_static_references() {
        let mut graph = parse_sources(&[
            Source {
                path: "main.tf".into(),
                text: r#"
resource "terraform_data" "a" {}
data "http" "d" {
  depends_on = ["terraform_data.a"]
}
resource "terraform_data" "b" {
  depends_on = ["terraform_data.a[0]", "data.http.d", "module.m", terraform_data.c]
}
output "o" {
  value      = 1
  depends_on = [" terraform_data.b "]
}
module "m" {
  source     = "./child"
  depends_on = ["terraform_data.a"]
}
"#
                .into(),
            },
            Source {
                path: "child/main.tf".into(),
                text: String::new(),
            },
        ]);
        project_modules(&mut graph, &AtomicBool::new(false)).unwrap();
        assert!(graph.gaps.is_empty());
        assert_eq!(
            dependencies(&graph),
            [
                ("data.http.d", "terraform_data.a"),
                ("module.m", "module.m.configuration.depends_on"),
                ("module.m.configuration.depends_on", "terraform_data.a"),
                ("output.o", "terraform_data.b"),
                ("terraform_data.b", "data.http.d"),
                ("terraform_data.b", "module.m"),
                ("terraform_data.b", "terraform_data.a"),
                ("terraform_data.b", "terraform_data.c"),
            ]
        );
    }

    #[test]
    fn import_blocks_report_unmodeled_coverage() {
        for (path, text) in [
            (
                "main.tf",
                "resource \"terraform_data\" \"item\" {}\nimport {\n  to = terraform_data.item\n  id = var.identifier\n}\n",
            ),
            (
                "main.tf.json",
                r#"{"resource": {"terraform_data": {"item": {}}},
"import": [{"to": "terraform_data.item", "id": "${var.identifier}"}]}"#,
            ),
        ] {
            let mut graph = parse_source(&Source {
                path: path.into(),
                text: text.into(),
            });
            project_modules(&mut graph, &AtomicBool::new(false)).unwrap();
            assert!(dependencies(&graph).is_empty(), "{path}");
            let gaps = graph
                .gaps
                .iter()
                .map(|gap| (gap.reason, gap.line_start, gap.line_end))
                .collect::<Vec<_>>();
            let lines = if path.ends_with(".json") {
                (2, 2)
            } else {
                (2, 5)
            };
            assert_eq!(
                gaps,
                [(
                    GapReason::TerraformImportUnmodeled,
                    Some(lines.0),
                    Some(lines.1)
                )],
                "{path}"
            );
            assert_eq!(graph.files[0].observed_relation_sites, 1, "{path}");
        }
    }

    #[test]
    fn provider_sources_with_ports_match_across_local_names() {
        let mut graph = parse_sources(&[
            Source {
                path: "main.tf".into(),
                text: r#"
terraform {
  required_providers {
    ops = { source = "LOCALHOST:1234/example/test" }
  }
}
provider "ops" {}
provider "ops" {
  alias = "second"
}
module "inherit" {
  source = "./child"
}
module "mapped" {
  source    = "./child"
  providers = { test = ops.second }
}
"#
                .into(),
            },
            Source {
                path: "child/main.tf".into(),
                text: r#"
terraform {
  required_providers {
    test = { source = "localhost:1234/example/test" }
  }
}
resource "test_thing" "x" {}
"#
                .into(),
            },
        ]);
        project_modules(&mut graph, &AtomicBool::new(false)).unwrap();
        assert!(graph.gaps.is_empty());
        assert_eq!(
            dependencies(&graph),
            [
                ("module.inherit", "module.inherit.test_thing.x"),
                // Default inheritance matches by source address, not local name.
                ("module.inherit.test_thing.x", "provider.ops"),
                ("module.mapped", "module.mapped.providers.test"),
                ("module.mapped", "module.mapped.test_thing.x"),
                ("module.mapped.providers.test", "provider.ops.second"),
                ("module.mapped.test_thing.x", "module.mapped.providers.test"),
            ]
        );
        // As in svchost, the default HTTPS port is not part of the identity.
        assert_eq!(
            normalized_provider_source("example.com:443/A/b"),
            Some("example.com/a/b".to_owned())
        );
    }

    #[test]
    fn unsupported_provider_source_reports_a_gap_instead_of_a_default() {
        let mut graph = parse_source(&Source {
            path: "main.tf".into(),
            text: r#"
terraform {
  required_providers {
    aws = { source = "example.com/a/b/c" }
  }
}
provider "aws" {}
resource "aws_instance" "x" {}
"#
            .into(),
        });
        project_modules(&mut graph, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            graph.gaps.iter().map(|gap| gap.reason).collect::<Vec<_>>(),
            [GapReason::TerraformProviderUnmodeled]
        );
    }
}
