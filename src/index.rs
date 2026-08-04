use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::git::{Repository, Source};
use crate::parse::{DefinitionKind, ParsedFile, RustParser};
use crate::store::{
    EdgeInput, EdgeKind, FileInput, Graph, NodeInput, NodeKind, RefInput, RefKind, Store,
};

const QUALIFIED_PATH_LIMIT: usize = 1024;

#[derive(Clone)]
pub struct Project {
    repository: Arc<Repository>,
}

impl Project {
    pub fn open(path: &Path) -> Result<Self, String> {
        Self::open_cancelled(path, &AtomicBool::new(false))
    }

    pub fn open_cancelled(path: &Path, cancelled: &AtomicBool) -> Result<Self, String> {
        Ok(Self {
            repository: Arc::new(Repository::discover_cancelled(path, cancelled)?),
        })
    }

    pub fn index(&self, rebuild: bool) -> Result<String, String> {
        self.index_cancelled(rebuild, Arc::new(AtomicBool::new(false)))
    }

    pub fn index_cancelled(
        &self,
        rebuild: bool,
        cancelled: Arc<AtomicBool>,
    ) -> Result<String, String> {
        check_cancelled(&cancelled)?;
        let mut store = Store::open(&self.repository.database, rebuild, &cancelled)?;
        let (state, changed, skipped) = store.index_with(&cancelled, |full, existing| {
            build_index(&self.repository, &cancelled, full, existing)
        })?;
        Ok(format!(
            "indexed generation={} changed={} skipped={}",
            state.generation, changed, skipped
        ))
    }

    pub fn search(&self, query: &str, kind: Option<&str>, limit: u32) -> Result<String, String> {
        let kind = match kind {
            None => None,
            Some("file") => Some(NodeKind::File),
            Some("type") => Some(NodeKind::Type),
            Some("function") => Some(NodeKind::Function),
            Some("test") => Some(NodeKind::Test),
            Some(_) => return Err("kind must be file, type, function, or test".into()),
        };
        Store::open_reader(&self.repository.database)?.search(query, kind, limit)
    }

    pub fn view(&self, node_ref: &str, depth: u32, max_nodes: u32) -> Result<String, String> {
        Store::open_reader(&self.repository.database)?.view(node_ref, depth, max_nodes)
    }

    pub fn changes_cancelled(
        &self,
        base: &str,
        depth: u32,
        max_nodes: u32,
        cancelled: Arc<AtomicBool>,
    ) -> Result<String, String> {
        check_cancelled(&cancelled)?;
        let changes = self.repository.worktree_changes(base, &cancelled)?;
        if changes.is_empty() {
            return Ok("no changes\n".into());
        }
        Store::open_reader(&self.repository.database)?
            .changes(&changes, depth, max_nodes, &cancelled)
    }
}

fn build_index(
    repository: &Repository,
    cancelled: &AtomicBool,
    full: bool,
    existing: &HashMap<String, crate::store::StoredFile>,
) -> Result<(Graph, usize), String> {
    let inventory = repository.rust_files(cancelled)?;
    let mut skipped = inventory.skipped;
    let mut parser = None;
    let mut targets = TargetLayout::discover(&repository.root);
    let mut graph = Graph::default();

    // ponytail: parse changed files sequentially; add Rayon only after profiling proves it helps.
    for file in &inventory.files {
        check_cancelled(cancelled)?;
        let target = targets.for_path(&file.path);
        let parse_context = target.parse_context();
        let old = existing.get(&file.path);
        if !full
            && old.is_some_and(|old| {
                old.parse_context == parse_context
                    && file
                        .git_oid
                        .as_ref()
                        .is_some_and(|oid| old.git_oid.as_ref() == Some(oid))
            })
        {
            let old = old.expect("checked above");
            graph.files.push(FileInput {
                path: file.path.clone(),
                git_oid: file.git_oid.clone(),
                content_hash: old.content_hash,
                parse_context,
                byte_size: old.byte_size,
                replace: false,
            });
            continue;
        }

        let Some(source) = repository.read_rust_source(file, cancelled)? else {
            skipped += 1;
            continue;
        };
        let content_hash = *blake3::hash(source.text.as_bytes()).as_bytes();
        let byte_size = u64::try_from(source.text.len())
            .map_err(|_| "source byte size exceeds supported range".to_owned())?;
        let changed = full
            || old.is_none_or(|old| {
                old.content_hash != content_hash || old.parse_context != parse_context
            });
        graph.files.push(FileInput {
            path: file.path.clone(),
            git_oid: file.git_oid.clone(),
            content_hash,
            parse_context,
            byte_size,
            replace: changed,
        });
        if changed {
            if parser.is_none() {
                parser = Some(RustParser::new()?);
            }
            add_file(
                &mut graph,
                &source,
                &target,
                parser.as_mut().expect("initialized above"),
            )?;
        }
    }
    if full {
        resolve(&mut graph, cancelled)?;
    }
    Ok((graph, skipped))
}

#[cfg(test)]
fn build_graph(sources: &[Source], cancelled: &AtomicBool) -> Result<Graph, String> {
    let mut parser = RustParser::new()?;
    let mut targets = TargetLayout::from_sources(sources);
    let mut graph = Graph {
        files: Vec::with_capacity(sources.len()),
        nodes: Vec::new(),
        refs: Vec::new(),
        edges: Vec::new(),
    };
    for source in sources {
        check_cancelled(cancelled)?;
        let target = targets.for_path(&source.path);
        graph.files.push(FileInput {
            path: source.path.clone(),
            git_oid: None,
            content_hash: *blake3::hash(source.text.as_bytes()).as_bytes(),
            parse_context: target.parse_context(),
            byte_size: u64::try_from(source.text.len())
                .map_err(|_| "source byte size exceeds supported range".to_owned())?,
            replace: true,
        });
        add_file(&mut graph, source, &target, &mut parser)?;
    }
    resolve(&mut graph, cancelled)?;
    Ok(graph)
}

fn add_file(
    graph: &mut Graph,
    source: &Source,
    target: &TargetPath,
    parser: &mut RustParser,
) -> Result<(), String> {
    let parsed = parser.parse(&source.text)?;

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
        line_end: line_count(&source.text)?,
        signature: String::new(),
        keys: vec![format!("rust:file:{}", source.path)],
    });

    let module = target.module.as_str();
    let module_paths = inline_module_paths(&parsed, module)?;
    let imports = import_bindings(&parsed, module, &module_paths, &target.root);
    let absolute_paths = definition_paths(&parsed, module, &module_paths, &target.root, &imports)?;
    let node_keys = parsed
        .definitions
        .iter()
        .enumerate()
        .map(|(local, definition)| {
            let kind = node_kind(definition.kind);
            identity(
                &source.path,
                kind_name(kind),
                absolute_paths[local].as_deref().unwrap_or(&definition.name),
                definition.line_start,
                local,
            )
        })
        .collect::<Vec<_>>();
    for (local, definition) in parsed.definitions.iter().enumerate() {
        let absolute = &absolute_paths[local];
        let parent_key = definition
            .parent
            .and_then(|parent| node_keys.get(parent).cloned())
            .unwrap_or_else(|| file_key.clone());
        let kind = node_kind(definition.kind);
        let keys = definition_keys(definition.kind, absolute.as_deref());
        let key = node_keys[local].clone();
        let owner_key = (definition.kind == DefinitionKind::Method
            && definition.impl_target.is_some()
            && definition.parent.is_none())
        .then(|| {
            absolute
                .as_deref()?
                .rsplit_once("::")
                .map(|(owner, _)| format!("rust:type:{owner}"))
        })
        .flatten();
        graph.nodes.push(NodeInput {
            key: key.clone(),
            file_key: source.path.clone(),
            kind,
            name: definition.name.clone(),
            qualified_name: key.clone(),
            parent_key: Some(parent_key),
            owner_key,
            line_start: to_u32(definition.line_start)?,
            line_end: to_u32(definition.line_end)?,
            signature: definition.signature.clone(),
            keys,
        });
    }

    let mut values = HashMap::<usize, HashSet<String>>::new();
    for binding in &parsed.bindings {
        values
            .entry(binding.source)
            .or_default()
            .insert(binding.name.clone());
    }
    let bindings = Bindings { imports, values };
    for import in &parsed.imports {
        let import_module = lexical_module(import.module, module, &module_paths);
        let Some(path) = normalize_use(&import.path, import_module, &target.root) else {
            continue;
        };
        graph.refs.push(RefInput {
            source_key: import
                .source
                .and_then(|source| node_keys.get(source).cloned())
                .unwrap_or_else(|| file_key.clone()),
            kind: RefKind::Imports,
            line: to_u32(import.line)?,
            keys: vec![item_key(&path)],
            resolved_target_key: None,
        });
    }

    for call in &parsed.calls {
        let Some(source_key) = node_keys.get(call.source) else {
            continue;
        };
        let keys = call_keys(
            &call.target,
            call.source,
            &parsed,
            &absolute_paths,
            target,
            &module_paths,
            &bindings,
        );
        if !keys.is_empty() {
            graph.refs.push(RefInput {
                source_key: source_key.clone(),
                kind: RefKind::Calls,
                line: to_u32(call.line)?,
                keys,
                resolved_target_key: None,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Candidate {
    Unique(usize),
    Ambiguous,
}

fn resolve(graph: &mut Graph, cancelled: &AtomicBool) -> Result<(), String> {
    let mut candidates = HashMap::new();
    for (node, input) in graph.nodes.iter().enumerate() {
        check_progress(node, cancelled)?;
        for key in &input.keys {
            candidates
                .entry(key.as_str())
                .and_modify(|candidate| {
                    if !matches!(candidate, Candidate::Unique(current) if *current == node) {
                        *candidate = Candidate::Ambiguous;
                    }
                })
                .or_insert(Candidate::Unique(node));
        }
    }

    let mut node_by_key = HashMap::with_capacity(graph.nodes.len());
    for (index, node) in graph.nodes.iter().enumerate() {
        check_progress(index, cancelled)?;
        node_by_key.insert(node.key.as_str(), index);
    }
    let mut parent_updates = Vec::new();
    for (node_index, node) in graph.nodes.iter().enumerate() {
        check_progress(node_index, cancelled)?;
        let Some(parent) = node.parent_key.as_deref() else {
            continue;
        };
        let Some(&parent_index) = node_by_key.get(parent) else {
            continue;
        };
        if graph.nodes[parent_index].kind != NodeKind::File {
            continue;
        }
        let Some(type_key) = node.owner_key.as_deref() else {
            continue;
        };
        let Some(Candidate::Unique(target)) = candidates.get(type_key) else {
            continue;
        };
        parent_updates.push((node_index, graph.nodes[*target].key.clone()));
    }
    let mut edge_indices = HashMap::<(String, String, u8), usize>::new();
    let mut edges = Vec::<EdgeInput>::new();
    for (index, reference) in graph.refs.iter_mut().enumerate() {
        check_progress(index, cancelled)?;
        let mut target = None;
        for key in &reference.keys {
            match candidates.get(key.as_str()) {
                Some(Candidate::Unique(node)) => {
                    target = Some(*node);
                    break;
                }
                Some(Candidate::Ambiguous) => break,
                None => {}
            }
        }
        let Some(target) = target else {
            continue;
        };
        let target_key = graph.nodes[target].key.clone();
        reference.resolved_target_key = Some(target_key.clone());

        let edge_kind = match reference.kind {
            RefKind::Imports => 2,
            RefKind::Calls => {
                if node_by_key
                    .get(reference.source_key.as_str())
                    .is_some_and(|source| graph.nodes[*source].kind == NodeKind::Test)
                {
                    1
                } else {
                    0
                }
            }
        };
        let key = (reference.source_key.clone(), target_key, edge_kind);
        if let Some(&edge) = edge_indices.get(&key) {
            edges[edge].support_count += 1;
        } else {
            let edge = edges.len();
            edge_indices.insert(key.clone(), edge);
            edges.push(EdgeInput {
                source_key: key.0,
                target_key: key.1,
                kind: match key.2 {
                    0 => EdgeKind::Calls,
                    1 => EdgeKind::TestCalls,
                    _ => EdgeKind::Imports,
                },
                support_count: 1,
            });
        }
    }
    graph.edges = edges;
    drop(node_by_key);
    drop(candidates);
    for (index, (node, parent_key)) in parent_updates.into_iter().enumerate() {
        check_progress(index, cancelled)?;
        graph.nodes[node].parent_key = Some(parent_key);
    }
    check_cancelled(cancelled)
}

fn definition_path(
    parsed: &ParsedFile,
    paths: &[Option<String>],
    local: usize,
    module: &str,
    module_paths: &[String],
    root: &str,
    imports: &ImportBindings,
) -> Result<Option<String>, String> {
    let Some(definition) = parsed.definitions.get(local) else {
        return Ok(None);
    };
    if let Some(parent) = definition.parent {
        return paths
            .get(parent)
            .and_then(Option::as_deref)
            .map(|parent| checked_join_path(parent, &definition.name))
            .transpose();
    }
    let module = lexical_module(definition.module, module, module_paths);
    let path = match definition.impl_target.as_deref() {
        Some(target) => normalize_impl_target(target, definition.module, module, root, imports)
            .map(|owner| checked_join_path(&owner, &definition.name))
            .transpose()?,
        None => Some(checked_join_path(module, &definition.name)?),
    };
    Ok(path)
}

fn definition_paths(
    parsed: &ParsedFile,
    module: &str,
    module_paths: &[String],
    root: &str,
    imports: &ImportBindings,
) -> Result<Vec<Option<String>>, String> {
    let mut paths = vec![None; parsed.definitions.len()];
    let mut state = vec![0_u8; parsed.definitions.len()];
    for start in 0..parsed.definitions.len() {
        if state[start] == 2 {
            continue;
        }
        let mut chain = Vec::new();
        let mut current = start;
        while state[current] == 0 {
            state[current] = 1;
            chain.push(current);
            let Some(parent) = parsed.definitions[current]
                .parent
                .filter(|parent| *parent < parsed.definitions.len())
            else {
                break;
            };
            current = parent;
        }
        if state[current] == 1 && chain.last() != Some(&current) {
            for definition in chain {
                state[definition] = 2;
            }
            continue;
        }
        while let Some(definition) = chain.pop() {
            paths[definition] = definition_path(
                parsed,
                &paths,
                definition,
                module,
                module_paths,
                root,
                imports,
            )?;
            state[definition] = 2;
        }
    }
    Ok(paths)
}

fn inline_module_paths(parsed: &ParsedFile, module: &str) -> Result<Vec<String>, String> {
    let mut paths = Vec::with_capacity(parsed.modules.len());
    for inline in &parsed.modules {
        let parent = inline
            .parent
            .and_then(|parent| paths.get(parent))
            .map_or(module, String::as_str);
        paths.push(checked_join_path(parent, &inline.name)?);
    }
    Ok(paths)
}

fn lexical_module<'a>(inline: Option<usize>, module: &'a str, paths: &'a [String]) -> &'a str {
    inline
        .and_then(|inline| paths.get(inline))
        .map_or(module, String::as_str)
}

fn definition_keys(kind: DefinitionKind, absolute: Option<&str>) -> Vec<String> {
    let Some(absolute) = absolute else {
        return Vec::new();
    };
    let mut keys = vec![item_key(absolute)];
    match kind {
        DefinitionKind::Type => keys.push(format!("rust:type:{absolute}")),
        DefinitionKind::Function | DefinitionKind::Test => {
            keys.push(format!("rust:function:{absolute}"));
        }
        DefinitionKind::Method => keys.push(format!("rust:method:{absolute}")),
    }
    keys
}

#[derive(Clone)]
enum Binding {
    Unique(String),
    Ambiguous,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct ImportScope {
    source: Option<usize>,
    module: Option<usize>,
}

type ImportBindings = HashMap<ImportScope, HashMap<String, Binding>>;

struct Bindings {
    imports: ImportBindings,
    values: HashMap<usize, HashSet<String>>,
}

fn import_bindings(
    parsed: &ParsedFile,
    module: &str,
    module_paths: &[String],
    root: &str,
) -> ImportBindings {
    let mut bindings = HashMap::with_capacity(parsed.imports.len());
    for import in &parsed.imports {
        if import.block_local && import.source.is_none() {
            continue;
        }
        let import_module = lexical_module(import.module, module, module_paths);
        let Some((alias, path)) = use_binding(&import.path, import_module, root) else {
            continue;
        };
        // ponytail: block-local imports stay unresolved until block ancestry is modeled.
        let candidate = if import.block_local {
            Binding::Ambiguous
        } else {
            Binding::Unique(path)
        };
        bindings
            .entry(ImportScope {
                source: import.source,
                module: import.module,
            })
            .or_insert_with(HashMap::new)
            .entry(alias)
            .and_modify(|binding| {
                if !matches!((&*binding, &candidate), (Binding::Unique(current), Binding::Unique(next)) if current == next)
                {
                    *binding = Binding::Ambiguous;
                }
            })
            .or_insert(candidate);
    }
    bindings
}

fn normalize_impl_target(
    raw: &str,
    module_index: Option<usize>,
    module: &str,
    root: &str,
    imports: &ImportBindings,
) -> Option<String> {
    let target = strip_trailing_type_arguments(raw.trim())?;
    let parts = target.split("::").map(str::trim).collect::<Vec<_>>();
    let first = *parts.first()?;
    if !matches!(first, "crate" | "self" | "super")
        && let Some(binding) = module_import_binding(imports, module_index, first)
    {
        return match binding {
            Binding::Unique(path) => Some(if parts.len() == 1 {
                path.clone()
            } else {
                join_path(path, &parts[1..].join("::"))
            }),
            Binding::Ambiguous => None,
        };
    }
    normalize_relative(&target, module, root)
}

fn module_import_binding<'a>(
    imports: &'a ImportBindings,
    module: Option<usize>,
    alias: &str,
) -> Option<&'a Binding> {
    imports
        .get(&ImportScope {
            source: None,
            module,
        })
        .and_then(|scope| scope.get(alias))
}

fn call_keys(
    raw: &str,
    source: usize,
    parsed: &ParsedFile,
    paths: &[Option<String>],
    target: &TargetPath,
    module_paths: &[String],
    bindings: &Bindings,
) -> Vec<String> {
    let definition = parsed.definitions.get(source);
    let module_index = definition.and_then(|definition| definition.module);
    let module = definition.map_or(target.module.as_str(), |definition| {
        lexical_module(definition.module, &target.module, module_paths)
    });
    let root = target.root.as_str();
    let Some(raw) = strip_generics(raw.trim()) else {
        return Vec::new();
    };
    let raw = raw.as_ref();
    if raw.is_empty() {
        return Vec::new();
    }

    if let Some(method) = raw.strip_prefix("self.") {
        return source_owner(source, parsed, paths)
            .map(|owner| vec![format!("rust:method:{}", join_path(owner, method))])
            .unwrap_or_default();
    }

    let parts = raw.split("::").map(str::trim).collect::<Vec<_>>();
    if parts.iter().any(|part| part.is_empty()) {
        return Vec::new();
    }
    if parts.len() == 1 {
        let name = parts[0];
        // ponytail: function-wide shadow suppression avoids false edges;
        // add block ranges only when the lost pre-binding edges matter.
        if bindings
            .values
            .get(&source)
            .is_some_and(|bindings| bindings.contains(name))
        {
            return vec![format!("rust:shadowed-value:{name}")];
        }
        if let Some(binding) = import_binding(&bindings.imports, source, module_index, name) {
            return match binding {
                Binding::Unique(path) => vec![format!("rust:function:{path}")],
                Binding::Ambiguous => vec![format!("rust:ambiguous-import:{name}")],
            };
        }
        let mut keys = Vec::with_capacity(2);
        if let Some(scope) = source_scope(source, parsed, paths, module) {
            keys.push(format!("rust:function:{}", join_path(scope, name)));
        }
        keys.push(format!("rust:function:{}", join_path(module, name)));
        return dedup_keys(keys);
    }

    let method = parts[parts.len() - 1];
    let owner = parts[..parts.len() - 1].join("::");
    if owner == "Self" {
        return source_owner(source, parsed, paths)
            .map(|owner| vec![format!("rust:method:{}", join_path(owner, method))])
            .unwrap_or_default();
    }

    let first = parts[0];
    let absolute_owner = match import_binding(&bindings.imports, source, module_index, first) {
        Some(Binding::Unique(path)) => Some(if parts.len() == 2 {
            path.clone()
        } else {
            join_path(path, &parts[1..parts.len() - 1].join("::"))
        }),
        Some(Binding::Ambiguous) => {
            return vec![format!("rust:ambiguous-import:{owner}::{method}")];
        }
        None => normalize_relative(&owner, module, root),
    };
    let mut keys = Vec::with_capacity(3);
    if let Some(owner) = absolute_owner {
        let target = join_path(&owner, method);
        keys.push(format!("rust:function:{target}"));
        keys.push(format!("rust:method:{target}"));
    }
    dedup_keys(keys)
}

fn import_binding<'a>(
    imports: &'a ImportBindings,
    source: usize,
    module: Option<usize>,
    alias: &str,
) -> Option<&'a Binding> {
    imports
        .get(&ImportScope {
            source: Some(source),
            module,
        })
        .and_then(|scope| scope.get(alias))
        .or_else(|| {
            imports
                .get(&ImportScope {
                    source: None,
                    module,
                })
                .and_then(|scope| scope.get(alias))
        })
}

fn source_scope<'a>(
    source: usize,
    parsed: &ParsedFile,
    paths: &'a [Option<String>],
    module: &'a str,
) -> Option<&'a str> {
    parsed.definitions[source]
        .parent
        .and_then(|parent| paths.get(parent))
        .and_then(Option::as_deref)
        .or(Some(module))
}

fn source_owner<'a>(
    source: usize,
    parsed: &ParsedFile,
    paths: &'a [Option<String>],
) -> Option<&'a str> {
    let definition = parsed.definitions.get(source)?;
    definition
        .parent
        .and_then(|parent| paths.get(parent))
        .and_then(Option::as_deref)
        .or_else(|| {
            let path = paths.get(source)?.as_deref()?;
            (definition.kind == DefinitionKind::Method)
                .then(|| path.rsplit_once("::").map(|(owner, _)| owner))
                .flatten()
        })
}

fn use_binding(raw: &str, module: &str, root: &str) -> Option<(String, String)> {
    let (path, alias) = raw
        .rsplit_once(" as ")
        .map_or((raw.trim(), None), |(path, alias)| {
            (path.trim(), Some(alias.trim()))
        });
    let absolute = normalize_use(path, module, root)?;
    let alias = alias.or_else(|| absolute.rsplit("::").next())?;
    let alias = alias.strip_prefix("r#").unwrap_or(alias).to_owned();
    Some((alias, absolute))
}

fn normalize_use(raw: &str, module: &str, root: &str) -> Option<String> {
    if raw.contains(['{', '}', '*']) {
        return None;
    }
    let path = raw.rsplit_once(" as ").map_or(raw, |(path, _)| path).trim();
    normalize_relative(path, module, root)
}

fn normalize_relative(raw: &str, module: &str, root: &str) -> Option<String> {
    let parts = raw
        .split("::")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let first = *parts.first()?;
    let root = split_path(root);
    let mut output = match first {
        "crate" => root.clone(),
        "self" => split_path(module),
        "super" => {
            let mut module = split_path(module);
            if module.len() <= root.len() {
                return None;
            }
            module.pop();
            module
        }
        _ => split_path(module),
    };
    let mut start = usize::from(matches!(first, "crate" | "self" | "super"));
    while parts.get(start) == Some(&"super") {
        if output.len() <= root.len() {
            return None;
        }
        output.pop();
        start += 1;
    }
    if parts[start..].iter().any(|part| !valid_identifier(part)) {
        return None;
    }
    output.extend(parts[start..].iter().map(|part| (*part).to_owned()));
    Some(output.join("::"))
}

#[derive(Debug, Eq, PartialEq)]
struct TargetPath {
    root: String,
    module: String,
}

impl TargetPath {
    fn parse_context(&self) -> String {
        format!("{}:{}{}", self.root.len(), self.root, self.module)
    }
}

#[derive(Clone, Copy, Default)]
struct PackageLayout {
    exists: bool,
    library: bool,
    main: bool,
}

struct TargetLayout {
    repository: Option<PathBuf>,
    packages: HashMap<String, PackageLayout>,
}

impl TargetLayout {
    fn discover(repository: &Path) -> Self {
        Self {
            repository: Some(repository.to_owned()),
            packages: HashMap::new(),
        }
    }

    #[cfg(test)]
    fn from_sources(sources: &[Source]) -> Self {
        let mut packages = HashMap::<String, PackageLayout>::new();
        packages.entry(String::new()).or_default().exists = true;
        for source in sources {
            for (suffix, library) in [("src/lib.rs", true), ("src/main.rs", false)] {
                let package = if source.path == suffix {
                    Some("")
                } else {
                    source.path.strip_suffix(&format!("/{suffix}"))
                };
                if let Some(package) = package {
                    let package = packages.entry(package.to_owned()).or_default();
                    package.exists = true;
                    if library {
                        package.library = true;
                    } else {
                        package.main = true;
                    }
                }
            }
        }
        Self {
            repository: None,
            packages,
        }
    }

    fn for_path(&mut self, path: &str) -> TargetPath {
        if let Some((package, relative)) = self.area(path, "src") {
            return src_target(package, relative, path, self.package(package));
        }
        for (area, kind) in [
            ("tests", "test"),
            ("examples", "example"),
            ("benches", "bench"),
        ] {
            if let Some((package, relative)) = self.area(path, area) {
                return external_target(package, relative, kind, path);
            }
        }
        if path == "build.rs" {
            return target(package_root(""), "build", "build.rs", "");
        }
        if let Some(package) = path.strip_suffix("/build.rs")
            && self.package(package).exists
        {
            return target(package_root(package), "build", "build.rs", "");
        }
        // ponytail: isolate custom Cargo paths per file; add manifest parsing when they matter.
        target(String::new(), "file", path, "")
    }

    fn area<'a>(&mut self, path: &'a str, area: &str) -> Option<(&'a str, &'a str)> {
        let prefix = format!("{area}/");
        if let Some(relative) = path.strip_prefix(&prefix)
            && self.package("").exists
        {
            return Some(("", relative));
        }
        let marker = format!("/{area}/");
        for (index, _) in path.rmatch_indices(&marker) {
            let package = &path[..index];
            if self.package(package).exists {
                return Some((package, &path[index + marker.len()..]));
            }
        }
        None
    }

    fn package(&mut self, package: &str) -> PackageLayout {
        if let Some(layout) = self.packages.get(package) {
            return *layout;
        }
        let Some(repository) = &self.repository else {
            return PackageLayout::default();
        };
        let root = repository.join(package);
        let exists = package.is_empty() || root.join("Cargo.toml").is_file();
        let layout = PackageLayout {
            exists,
            library: exists && root.join("src/lib.rs").is_file(),
            main: exists && root.join("src/main.rs").is_file(),
        };
        self.packages.insert(package.to_owned(), layout);
        layout
    }
}

fn src_target(package: &str, relative: &str, path: &str, layout: PackageLayout) -> TargetPath {
    let package_root = package_root(package);
    match relative {
        "lib.rs" => TargetPath {
            module: package_root.clone(),
            root: package_root,
        },
        "main.rs" => target(package_root, "main", "main.rs", ""),
        _ => {
            if let Some(bin) = relative.strip_prefix("bin/") {
                if let Some((name, rest)) = bin.split_once('/') {
                    let module = if rest == "main.rs" {
                        String::new()
                    } else if let Some(module) = file_module(rest) {
                        module
                    } else {
                        return target(String::new(), "file", path, "");
                    };
                    return target(package_root, "bin", name, &module);
                }
                if let Some(name) = bin.strip_suffix(".rs") {
                    return target(package_root, "bin", name, "");
                }
                return target(String::new(), "file", path, "");
            }
            let Some(module) = file_module(relative).filter(|module| !module.is_empty()) else {
                return target(String::new(), "file", path, "");
            };
            let package_root = if layout.library {
                // ponytail: a lib+bin package assigns shared src modules to the library;
                // add module ownership expansion when multi-target contexts are needed.
                package_root
            } else if layout.main {
                target(package_root, "main", "main.rs", "").root
            } else {
                return target(String::new(), "file", path, "");
            };
            TargetPath {
                module: join_path(&package_root, &module),
                root: package_root,
            }
        }
    }
}

fn external_target(package: &str, relative: &str, kind: &str, path: &str) -> TargetPath {
    let package_root = package_root(package);
    if let Some(name) = relative
        .strip_suffix(".rs")
        .filter(|name| !name.contains('/'))
    {
        return target(package_root, kind, name, "");
    }
    let Some((name, rest)) = relative.split_once('/') else {
        return target(String::new(), "file", path, "");
    };
    let module = if rest == "main.rs" {
        String::new()
    } else if let Some(module) = file_module(rest) {
        module
    } else {
        return target(String::new(), "file", path, "");
    };
    target(package_root, kind, name, &module)
}

fn target(package_root: String, kind: &str, name: &str, module: &str) -> TargetPath {
    let root = join_path(&package_root, &internal_component(kind, name));
    TargetPath {
        module: join_path(&root, module),
        root,
    }
}

fn package_root(package: &str) -> String {
    if package.is_empty() {
        String::new()
    } else {
        internal_component("pkg", package)
    }
}

fn internal_component(kind: &str, value: &str) -> String {
    format!("@{kind}:{}:{value}", value.len())
}

fn file_module(relative: &str) -> Option<String> {
    let (parent, file) = relative.rsplit_once('/').unwrap_or(("", relative));
    let stem = file.strip_suffix(".rs")?;
    Some(if stem == "mod" {
        parent.replace('/', "::")
    } else {
        join_path(&parent.replace('/', "::"), stem)
    })
}

fn identity(path: &str, kind: &str, scope: &str, line: usize, ordinal: usize) -> String {
    format!(
        "rust:{}#{path}:{}#{kind}:{}#{scope}:{line}:{ordinal}",
        path.len(),
        kind.len(),
        scope.len()
    )
}

fn item_key(path: &str) -> String {
    format!("rust:item:{path}")
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

fn checked_join_path(left: &str, right: &str) -> Result<String, String> {
    let separator = usize::from(!left.is_empty() && !right.is_empty()) * 2;
    left.len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(right.len()))
        .filter(|length| *length <= QUALIFIED_PATH_LIMIT)
        .ok_or_else(|| "Rust qualified path exceeds 1024 bytes".to_owned())?;
    Ok(join_path(left, right))
}

fn split_path(path: &str) -> Vec<String> {
    path.split("::")
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn valid_identifier(value: &str) -> bool {
    let value = value.strip_prefix("r#").unwrap_or(value);
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && chars.all(|character| character == '_' || character.is_alphanumeric())
}

fn strip_trailing_type_arguments(value: &str) -> Option<Cow<'_, str>> {
    let Some(start) = value.find('<') else {
        return Some(Cow::Borrowed(value));
    };
    let mut depth = 0_usize;
    let mut end = None;
    for (offset, character) in value[start..].char_indices() {
        match character {
            '<' => depth += 1,
            '>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    end = Some(start + offset + character.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    if !value[end..].trim().is_empty() {
        return None;
    }
    let path = value[..start].trim_end();
    (!path.is_empty()).then_some(Cow::Borrowed(path))
}

fn strip_generics(value: &str) -> Option<Cow<'_, str>> {
    if !value.contains("::<") {
        return Some(Cow::Borrowed(value));
    }
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("::<") {
        output.push_str(&rest[..start]);
        let mut depth = 0_usize;
        let mut end = None;
        for (offset, character) in rest[start + 2..].char_indices() {
            match character {
                '<' => depth += 1,
                '>' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        end = Some(start + 2 + offset + character.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &rest[end?..];
    }
    output.push_str(rest);
    Some(Cow::Owned(output))
}

fn dedup_keys(keys: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut unique = Vec::new();
    for key in keys {
        if !unique.contains(&key) {
            unique.push(key);
        }
    }
    unique
}

fn node_kind(kind: DefinitionKind) -> NodeKind {
    match kind {
        DefinitionKind::Type => NodeKind::Type,
        DefinitionKind::Function | DefinitionKind::Method => NodeKind::Function,
        DefinitionKind::Test => NodeKind::Test,
    }
}

const fn kind_name(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::File => "file",
        NodeKind::Type => "type",
        NodeKind::Function => "function",
        NodeKind::Test => "test",
    }
}

fn line_count(source: &str) -> Result<u32, String> {
    to_u32(source.lines().count().max(1))
}

fn to_u32(value: usize) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| "source line exceeds supported range".into())
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("index cancelled".into())
    } else {
        Ok(())
    }
}

fn check_progress(index: usize, cancelled: &AtomicBool) -> Result<(), String> {
    if index & 1023 == 0 {
        check_cancelled(cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_cross_file_exact_keys_and_test_calls() {
        let sources = [
            Source {
                path: "src/lib.rs".into(),
                text: r#"mod mailer;
use crate::mailer::Mailer;
fn register() { Mailer::dispatch(); }
#[test]
fn register_dispatches() { register(); }
"#
                .into(),
            },
            Source {
                path: "src/mailer.rs".into(),
                text: "pub struct Mailer; impl Mailer { pub fn dispatch(&self) { self.flush(); } fn flush(&self) {} }".into(),
            },
        ];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();

        let dispatch = graph
            .nodes
            .iter()
            .find(|node| node.name == "dispatch")
            .unwrap();
        assert!(
            dispatch
                .keys
                .contains(&"rust:method:mailer::Mailer::dispatch".into())
        );
        assert!(graph.refs.iter().any(|reference| {
            reference
                .keys
                .contains(&"rust:method:mailer::Mailer::dispatch".into())
        }));
        assert!(graph.refs.iter().any(|reference| {
            reference
                .keys
                .first()
                .is_some_and(|key| key == "rust:method:mailer::Mailer::flush")
                && reference.resolved_target_key.is_some()
        }));
        let mailer = graph
            .nodes
            .iter()
            .find(|node| node.name == "Mailer")
            .unwrap();
        assert!(
            graph
                .nodes
                .iter()
                .filter(|node| matches!(node.name.as_str(), "dispatch" | "flush"))
                .all(|node| node.parent_key.as_deref() == Some(mailer.key.as_str()))
        );

        let test = graph
            .nodes
            .iter()
            .position(|node| node.kind == NodeKind::Test)
            .unwrap();
        assert!(graph.refs.iter().any(|reference| {
            reference.source_key == graph.nodes[test].key
                && reference.keys.first().is_some_and(|key| {
                    key == "rust:function:register_dispatches::register"
                        || key == "rust:function:register"
                })
                && reference.resolved_target_key.is_some()
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.source_key == graph.nodes[test].key && edge.kind == EdgeKind::TestCalls
        }));
    }

    #[test]
    fn ambiguous_import_aliases_do_not_fall_back() {
        let parsed = RustParser::new()
            .unwrap()
            .parse("use crate::first::Item; use crate::second::Item; fn run() { Item::go(); }")
            .unwrap();
        let module_paths = inline_module_paths(&parsed, "").unwrap();
        let imports = import_bindings(&parsed, "", &module_paths, "");
        let paths = parsed
            .definitions
            .iter()
            .enumerate()
            .map(|(index, _)| definition_path(&parsed, &[], index, "", &module_paths, "", &imports))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let bindings = Bindings {
            imports,
            values: HashMap::new(),
        };
        let target = TargetPath {
            root: String::new(),
            module: String::new(),
        };
        assert_eq!(
            call_keys(
                "Item::go",
                0,
                &parsed,
                &paths,
                &target,
                &module_paths,
                &bindings,
            ),
            ["rust:ambiguous-import:Item::go"]
        );
    }

    #[test]
    fn unqualified_calls_do_not_guess_across_modules() {
        let sources = [
            Source {
                path: "src/a.rs".into(),
                text: "fn duplicate() {}".into(),
            },
            Source {
                path: "src/b.rs".into(),
                text: "fn duplicate() {}".into(),
            },
            Source {
                path: "src/lib.rs".into(),
                text: "fn caller() { duplicate(); }".into(),
            },
        ];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();
        let reference = graph
            .refs
            .iter()
            .find(|reference| reference.keys == ["rust:function:duplicate"])
            .unwrap();

        assert!(reference.resolved_target_key.is_none());
    }

    #[test]
    fn bounds_deep_qualified_paths() {
        let depth = 400;
        let sources = [Source {
            path: "src/lib.rs".into(),
            text: format!(
                "{}fn leaf() {{}}{}",
                "mod m {{".repeat(depth),
                "}".repeat(depth)
            ),
        }];

        assert_eq!(
            build_graph(&sources, &AtomicBool::new(false))
                .err()
                .unwrap(),
            "Rust qualified path exceeds 1024 bytes"
        );
    }

    #[test]
    fn local_values_shadow_unqualified_function_calls() {
        let sources = [Source {
            path: "src/lib.rs".into(),
            text: r#"
fn helper() {}
fn by_parameter(helper: fn()) { helper(); }
fn by_let() { let helper = || {}; helper(); }
fn by_if(value: Option<fn()>) { if let Some(helper) = value { helper(); } }
fn by_for(values: Vec<fn()>) { for helper in values { helper(); } }
fn by_match(value: Option<fn()>) { match value { Some(helper) => helper(), None => {} } }
fn by_closure() { let _callback = |(helper,)| helper(); }
fn by_const() { const helper: fn() = helper; helper(); }
fn by_static() { static helper: fn() = helper; helper(); }
"#
            .into(),
        }];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();

        for name in [
            "by_parameter",
            "by_let",
            "by_if",
            "by_for",
            "by_match",
            "by_closure",
            "by_const",
            "by_static",
        ] {
            let source = graph.nodes.iter().find(|node| node.name == name).unwrap();
            assert!(graph.refs.iter().any(|reference| {
                reference.source_key == source.key
                    && reference
                        .keys
                        .iter()
                        .any(|key| key == "rust:shadowed-value:helper")
                    && reference.resolved_target_key.is_none()
            }));
        }
    }

    #[test]
    fn resolves_imported_and_scoped_impl_owners() {
        let sources = [Source {
            path: "src/lib.rs".into(),
            text: r#"
mod model { pub struct Item; }
mod imported {
    use crate::model::Item as Imported;
    impl Imported { fn run() {} }
    fn call() { Imported::run(); }
}
mod scoped { impl crate::model::Item { fn stop() {} } }
"#
            .into(),
        }];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();
        let item = graph
            .nodes
            .iter()
            .find(|node| node.keys.contains(&"rust:type:model::Item".into()))
            .unwrap();

        for (name, key) in [
            ("run", "rust:method:model::Item::run"),
            ("stop", "rust:method:model::Item::stop"),
        ] {
            let method = graph.nodes.iter().find(|node| node.name == name).unwrap();
            assert!(method.keys.contains(&key.into()));
            assert_eq!(method.parent_key.as_deref(), Some(item.key.as_str()));
        }
        let call = graph.nodes.iter().find(|node| node.name == "call").unwrap();
        assert!(graph.refs.iter().any(|reference| {
            reference.source_key == call.key
                && reference.resolved_target_key
                    == graph
                        .nodes
                        .iter()
                        .find(|node| node.name == "run")
                        .map(|node| node.key.clone())
        }));
    }

    #[test]
    fn scopes_imports_and_keeps_explicit_aliases_authoritative() {
        let sources = [Source {
            path: "src/lib.rs".into(),
            text: r#"
mod a { pub struct Thing; impl Thing { pub fn go() {} } pub fn run() {} }
mod c { pub struct Thing; impl Thing { pub fn go() {} } pub fn execute() {} }
mod first_scope { use crate::a::Thing; fn first() { Thing::go(); } }
mod second_scope { use crate::c::Thing; fn second() { Thing::go(); } }
mod callers {
    use crate::a::run as execute;
    fn aliased() { execute(); }
    use dep::Client as External;
    struct External;
    impl External { fn new() {} }
    fn external() { External::new(); }
    fn local_import() { { use crate::a::Thing; } Thing::go(); }
}
mod anonymous {
    const _: () = { use crate::a::run as local; };
    fn local() {}
    fn after_const() { local(); }
}
"#
            .into(),
        }];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();
        let target = |key: &str| {
            graph
                .nodes
                .iter()
                .find(|node| node.keys.iter().any(|candidate| candidate == key))
                .map(|node| node.key.as_str())
                .unwrap()
        };

        for (source, expected) in [
            ("first", "rust:method:a::Thing::go"),
            ("second", "rust:method:c::Thing::go"),
            ("aliased", "rust:function:a::run"),
            ("after_const", "rust:function:anonymous::local"),
        ] {
            let source = graph.nodes.iter().find(|node| node.name == source).unwrap();
            assert!(graph.refs.iter().any(|reference| {
                reference.source_key == source.key
                    && reference.resolved_target_key.as_deref() == Some(target(expected))
            }));
        }

        let external = graph
            .nodes
            .iter()
            .find(|node| node.name == "external")
            .unwrap();
        assert!(graph.refs.iter().any(|reference| {
            reference.source_key == external.key && reference.resolved_target_key.is_none()
        }));
        let local_import = graph
            .nodes
            .iter()
            .find(|node| node.name == "local_import")
            .unwrap();
        assert!(graph.refs.iter().any(|reference| {
            reference.source_key == local_import.key && reference.resolved_target_key.is_none()
        }));
    }

    #[test]
    fn resolves_scoped_calls_and_forward_impl_parents_without_name_collisions() {
        let sources = [
            Source {
                path: "src/lib.rs".into(),
                text: "mod jobs; impl Thing { fn go(&self) { struct Nested; } } struct Thing; fn duplicate() {} fn duplicate() {} fn caller() { crate::jobs::run(); }".into(),
            },
            Source {
                path: "src/jobs.rs".into(),
                text: "pub fn run() {}".into(),
            },
        ];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();

        let thing = graph
            .nodes
            .iter()
            .find(|node| node.name == "Thing")
            .unwrap();
        let method = graph.nodes.iter().find(|node| node.name == "go").unwrap();
        assert_eq!(method.parent_key.as_deref(), Some(thing.key.as_str()));
        assert!(method.keys.contains(&"rust:method:Thing::go".into()));
        let nested = graph
            .nodes
            .iter()
            .find(|node| node.name == "Nested")
            .unwrap();
        assert!(nested.keys.contains(&"rust:item:Thing::go::Nested".into()));

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        assert!(graph.refs.iter().any(|reference| {
            reference.keys.first() == Some(&"rust:function:jobs::run".into())
                && reference.resolved_target_key.as_deref() == Some(run.key.as_str())
        }));

        let duplicates = graph
            .nodes
            .iter()
            .filter(|node| node.name == "duplicate")
            .collect::<Vec<_>>();
        assert_eq!(duplicates.len(), 2);
        assert_ne!(duplicates[0].qualified_name, duplicates[1].qualified_name);
    }

    #[test]
    fn resolves_inline_module_and_root_calls_with_exact_keys() {
        let sources = [Source {
            path: "src/lib.rs".into(),
            text: "mod a { pub fn run() {} fn local() { run(); } } mod b { pub fn run() {} } fn root() {} fn caller() { crate::a::run(); crate::root(); }".into(),
        }];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();

        let a_run = graph
            .nodes
            .iter()
            .find(|node| node.keys.contains(&"rust:function:a::run".into()))
            .unwrap();
        assert!(graph.refs.iter().any(|reference| {
            reference.keys.first() == Some(&"rust:function:a::run".into())
                && reference.resolved_target_key.as_deref() == Some(a_run.key.as_str())
        }));
        assert!(graph.refs.iter().any(|reference| {
            reference.keys.first() == Some(&"rust:function:root".into())
                && reference.resolved_target_key.is_some()
        }));
    }

    #[test]
    fn classifies_conventional_crate_targets_without_changing_root_lib_paths() {
        let mut targets = test_targets(&[
            "src/lib.rs",
            "src/main.rs",
            "src/worker.rs",
            "src/bin/tool.rs",
            "src/bin/tool/main.rs",
            "src/bin/tool/helper.rs",
            "crates/app/src/lib.rs",
            "crates/app/src/worker.rs",
            "crates/other/src/lib.rs",
            "crates/other/src/worker.rs",
        ]);
        assert_eq!(
            targets.for_path("src/lib.rs"),
            TargetPath {
                root: String::new(),
                module: String::new(),
            }
        );
        assert_eq!(targets.for_path("src/worker.rs").module, "worker");

        let app = targets.for_path("crates/app/src/lib.rs");
        let worker = targets.for_path("crates/app/src/worker.rs");
        let other = targets.for_path("crates/other/src/worker.rs");
        assert!(!app.root.is_empty());
        assert_eq!(app.root, worker.root);
        assert_eq!(worker.module, join_path(&app.root, "worker"));
        assert_ne!(worker.root, other.root);

        let main = targets.for_path("src/main.rs");
        let bin = targets.for_path("src/bin/tool.rs");
        let bin_directory = targets.for_path("src/bin/tool/main.rs");
        let bin_helper = targets.for_path("src/bin/tool/helper.rs");
        assert_ne!(main.root, bin.root);
        assert_eq!(bin.root, bin_directory.root);
        assert_eq!(bin_helper.module, join_path(&bin.root, "helper"));

        let mut roots = [
            main.root,
            bin.root,
            targets.for_path("tests/check.rs").root,
            targets.for_path("examples/check.rs").root,
            targets.for_path("benches/check.rs").root,
            targets.for_path("build.rs").root,
        ];
        roots.sort();
        assert!(roots.windows(2).all(|pair| pair[0] != pair[1]));
        assert_ne!(
            targets.for_path("custom/main.rs").root,
            targets.for_path("custom/helper.rs").root
        );

        assert_eq!(
            targets.for_path("tests/suite/main.rs").module,
            targets.for_path("tests/suite/helper.rs").root
        );

        let mut binary = test_targets(&["src/main.rs", "src/worker.rs"]);
        assert_eq!(
            binary.for_path("src/main.rs").root,
            binary.for_path("src/worker.rs").root
        );

        let mut nested = test_targets(&[
            "crates/app/src/lib.rs",
            "crates/app/src/generated/src/task.rs",
        ]);
        assert!(
            nested
                .for_path("crates/app/src/generated/src/task.rs")
                .module
                .ends_with("::generated::src::task")
        );
    }

    #[test]
    fn relative_paths_stop_at_their_target_root() {
        let mut targets =
            test_targets(&["crates/app/src/lib.rs", "crates/app/src/worker/nested.rs"]);
        let target = targets.for_path("crates/app/src/worker/nested.rs");
        assert_eq!(
            normalize_relative("crate::root", &target.module, &target.root),
            Some(join_path(&target.root, "root"))
        );
        assert_eq!(
            normalize_relative("self::local", &target.module, &target.root),
            Some(join_path(&target.module, "local"))
        );
        assert_eq!(
            normalize_relative("super::peer", &target.module, &target.root),
            Some(join_path(
                target.module.rsplit_once("::").unwrap().0,
                "peer"
            ))
        );
        assert_eq!(
            normalize_relative("super::super::top", &target.module, &target.root),
            Some(join_path(&target.root, "top"))
        );
        assert_eq!(
            normalize_relative("super::super::super::escape", &target.module, &target.root),
            None
        );
        assert_eq!(
            normalize_relative("super::root", "worker", ""),
            Some("root".into())
        );
        assert_eq!(
            strip_generics("Vec::<Option<u8>>::new").as_deref(),
            Some("Vec::new")
        );
        assert!(strip_generics("Vec::<u8").is_none());
    }

    #[test]
    fn calls_do_not_fall_back_across_crate_targets() {
        let sources = [
            Source {
                path: "crates/app/src/lib.rs".into(),
                text: "fn caller() { borrowed(); }".into(),
            },
            Source {
                path: "crates/other/src/lib.rs".into(),
                text: "fn borrowed() {}".into(),
            },
        ];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();
        let reference = graph.refs.first().unwrap();

        assert!(reference.resolved_target_key.is_none());
        assert!(
            reference
                .keys
                .iter()
                .all(|key| key != "rust:function-name:borrowed")
        );
    }

    #[test]
    fn scopes_workspace_crates_before_resolving_calls() {
        let unique = [
            Source {
                path: "crates/app/src/lib.rs".into(),
                text: "fn caller() { crate::worker::work(); }".into(),
            },
            Source {
                path: "crates/app/src/worker.rs".into(),
                text: "pub fn work() {}".into(),
            },
        ];
        let mut targets = TargetLayout::from_sources(&unique);
        let app_work = format!(
            "rust:function:{}::work",
            targets.for_path("crates/app/src/worker.rs").module
        );
        let graph = build_graph(&unique, &AtomicBool::new(false)).unwrap();
        assert!(graph.refs.iter().any(|reference| {
            reference.keys.first() == Some(&app_work) && reference.resolved_target_key.is_some()
        }));

        let duplicate = [
            Source {
                path: "crates/app/src/lib.rs".into(),
                text: "fn caller() { crate::worker::work(); }".into(),
            },
            Source {
                path: "crates/app/src/worker.rs".into(),
                text: "pub fn work() {}".into(),
            },
            Source {
                path: "crates/other/src/worker.rs".into(),
                text: "pub fn work() {}".into(),
            },
        ];
        let graph = build_graph(&duplicate, &AtomicBool::new(false)).unwrap();
        let app_target = graph
            .nodes
            .iter()
            .find(|node| node.file_key == "crates/app/src/worker.rs" && node.name == "work")
            .unwrap();
        assert!(graph.refs.iter().any(|reference| {
            reference.keys.first() == Some(&app_work)
                && reference.resolved_target_key.as_deref() == Some(app_target.key.as_str())
        }));
        let work_keys = graph
            .nodes
            .iter()
            .filter(|node| node.name == "work")
            .map(|node| &node.keys)
            .collect::<Vec<_>>();
        assert_ne!(work_keys[0], work_keys[1]);
    }

    fn test_targets(paths: &[&str]) -> TargetLayout {
        let sources = paths
            .iter()
            .map(|path| Source {
                path: (*path).to_owned(),
                text: String::new(),
            })
            .collect::<Vec<_>>();
        TargetLayout::from_sources(&sources)
    }
}
