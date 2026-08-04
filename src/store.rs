use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::git::{ChangedFile, LineSpan, PathRecord, WorktreeChanges};

const SCHEMA_VERSION: i64 = 2;
const SEARCH_BUDGET: usize = 1536;
const VIEW_BUDGET: usize = 4096;
const CHANGES_BUDGET: usize = 8192;
const TRUNCATED: &str = "[truncated]\n";
const BUSY_LIMIT: Duration = Duration::from_secs(5);
const BUSY_POLL: Duration = Duration::from_millis(5);

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    File,
    Type,
    Function,
    Test,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefKind {
    Calls,
    Imports,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EdgeKind {
    Calls,
    TestCalls,
    Imports,
}

pub struct FileInput {
    pub path: String,
    pub git_oid: Option<String>,
    pub content_hash: [u8; 32],
    pub parse_context: String,
    pub byte_size: u64,
    pub replace: bool,
}

pub struct StoredFile {
    id: i64,
    pub git_oid: Option<String>,
    pub content_hash: [u8; 32],
    pub parse_context: String,
    pub byte_size: u64,
}

pub struct NodeInput {
    pub key: String,
    pub file_key: String,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub parent_key: Option<String>,
    pub owner_key: Option<String>,
    pub line_start: u32,
    pub line_end: u32,
    pub signature: String,
    pub keys: Vec<String>,
}

pub struct RefInput {
    pub source_key: String,
    pub kind: RefKind,
    pub line: u32,
    pub keys: Vec<String>,
    pub resolved_target_key: Option<String>,
}

pub struct EdgeInput {
    pub source_key: String,
    pub target_key: String,
    pub kind: EdgeKind,
    pub support_count: u32,
}

#[derive(Default)]
pub struct Graph {
    pub files: Vec<FileInput>,
    pub nodes: Vec<NodeInput>,
    pub refs: Vec<RefInput>,
    pub edges: Vec<EdgeInput>,
}

pub struct State {
    pub epoch: String,
    pub generation: i64,
}

pub struct Store {
    connection: Connection,
    rebuild: bool,
}

impl Store {
    pub fn open(path: &Path, rebuild: bool, cancelled: &AtomicBool) -> Result<Self> {
        check_cancelled(cancelled)?;
        let parent = path
            .parent()
            .ok_or_else(|| "database path has no parent".to_owned())?;
        match fs::create_dir(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("cannot create database directory: {error}")),
        }
        let metadata = fs::symlink_metadata(parent)
            .map_err(|error| format!("cannot inspect database directory: {error}"))?;
        if !metadata.is_dir() {
            return Err("database directory is not a regular directory".into());
        }

        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(path, flags)
            .map_err(|error| format!("cannot open database {}: {error}", path.display()))?;
        connection.busy_timeout(Duration::ZERO).map_err(db_error)?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(db_error)?;
        verify_sqlite()?;
        let version: i64 = retry_sqlite(cancelled, || {
            connection.pragma_query_value(None, "user_version", |row| row.get(0))
        })?;
        if !matches!(version, 0 | SCHEMA_VERSION) && !rebuild {
            return Err("database schema mismatch; run index --rebuild".into());
        }
        configure_journal(&connection, cancelled)?;

        let store = Self {
            connection,
            rebuild,
        };
        match version {
            0 => {}
            SCHEMA_VERSION => read_state_cancelled(&store.connection, cancelled).map(|_| ())?,
            _ if rebuild => {}
            _ => unreachable!("schema mismatch returned above"),
        }
        Ok(store)
    }

    pub fn open_reader(path: &Path) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(path, flags)
            .map_err(|error| format!("cannot open database {}: {error}", path.display()))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        verify_sqlite()?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        if version != SCHEMA_VERSION {
            return Err("database schema mismatch; run index --rebuild".into());
        }
        Ok(Self {
            connection,
            rebuild: false,
        })
    }

    pub fn index_with<T>(
        &mut self,
        cancelled: &AtomicBool,
        build: impl FnOnce(bool, &HashMap<String, StoredFile>) -> Result<(Graph, T)>,
    ) -> Result<(State, usize, T)> {
        let tx = begin_immediate(&self.connection, cancelled)?;
        check_cancelled(cancelled)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        let new_schema = version == 0;
        let rebuild_schema = !new_schema && self.rebuild;
        if !new_schema && !rebuild_schema && version != SCHEMA_VERSION {
            return Err("database schema mismatch; run index --rebuild".into());
        }
        if !new_schema {
            read_state(&tx)?;
        }
        let full = new_schema || rebuild_schema;
        let existing = if full {
            HashMap::new()
        } else {
            load_stored_files(&tx)?
        };
        let (graph, value) = build(full, &existing)?;
        check_cancelled(cancelled)?;

        let changed = if full {
            if new_schema {
                create_schema(&tx)?;
            } else {
                drop_graph_schema(&tx)?;
                create_schema(&tx)?;
            }
            insert_graph(&tx, &graph, cancelled, false)?;
            graph.files.len()
        } else {
            apply_incremental(&tx, &graph, &existing, cancelled)?
        };
        if full || changed != 0 {
            tx.execute(
                "UPDATE state SET generation=generation+1 WHERE singleton=1",
                [],
            )
            .map_err(db_error)?;
        }
        let state = read_state(&tx)?;
        check_cancelled(cancelled)?;
        tx.commit().map_err(db_error)?;
        self.rebuild = false;
        Ok((state, changed, value))
    }

    pub fn search(&mut self, query: &str, kind: Option<NodeKind>, limit: u32) -> Result<String> {
        if query.trim().is_empty() || query.len() > 256 || !(1..=20).contains(&limit) {
            return Err("invalid search parameters".into());
        }
        let fts = literal_fts(query)?;
        let tx = self.connection.transaction().map_err(db_error)?;
        let state = read_state(&tx)?;
        let mut statement = tx
            .prepare(
                "SELECT n.id, n.kind, n.name, f.path, n.line_start
                 FROM nodes_fts
                 JOIN nodes n ON n.id=nodes_fts.rowid
                 JOIN files f ON f.id=n.file_id
                 WHERE nodes_fts MATCH ?1 AND (?2 IS NULL OR n.kind=?2)
                 ORDER BY bm25(nodes_fts), n.qualified_name, n.id
                 LIMIT ?3",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map(params![fts, kind.map(NodeKind::db), limit + 1], |row| {
                Ok(RowNode {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    path: row.get(3)?,
                    line: row.get(4)?,
                })
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        if rows.is_empty() {
            return Ok("no matches\n".into());
        }
        let omitted = rows.len() > limit as usize;
        let mut lines = Vec::with_capacity(rows.len().min(limit as usize));
        let mut omitted = omitted;
        for node in rows.iter().take(limit as usize) {
            let Some(line) = node.line(&state, None, SEARCH_BUDGET)? else {
                omitted = true;
                break;
            };
            lines.push(line);
        }
        Ok(bounded(lines, SEARCH_BUDGET, omitted))
    }

    pub fn view(&mut self, node_ref: &str, depth: u32, max_nodes: u32) -> Result<String> {
        if depth > 3 || !(1..=50).contains(&max_nodes) || node_ref.len() > 128 {
            return Err("invalid view parameters".into());
        }
        let (epoch, generation, root_id) = parse_ref(node_ref)?;
        let tx = self.connection.transaction().map_err(db_error)?;
        let state = read_state(&tx)?;
        if epoch != state.epoch || generation != state.generation {
            return Err("stale node_ref".into());
        }
        let root = load_node(&tx, root_id)?.ok_or_else(|| "node not found".to_owned())?;
        let Some(root_line) = root.line(&state, None, VIEW_BUDGET)? else {
            return Ok(TRUNCATED.into());
        };
        let root_has_members = matches!(root.kind.as_str(), "file" | "type");
        let mut lines = vec![root_line];
        let mut visited = HashSet::from([root_id]);
        let mut queue = VecDeque::from([(root_id, 0_u32)]);
        let mut row_budget = max_nodes as usize + 1;
        let mut omitted = false;

        while let Some((current, level)) = queue.pop_front() {
            if row_budget == 0 {
                omitted = true;
                break;
            }
            let (neighbors, more_neighbors) = load_neighbors(
                &tx,
                current,
                row_budget,
                current == root_id && root_has_members,
            )?;
            for (relation, node) in neighbors {
                row_budget -= 1;
                if visited.contains(&node.id) {
                    continue;
                }
                if level >= depth || visited.len() >= max_nodes as usize {
                    omitted = true;
                    break;
                }
                visited.insert(node.id);
                let Some(line) = node.line(&state, Some(&relation), VIEW_BUDGET)? else {
                    omitted = true;
                    break;
                };
                lines.push(line);
                queue.push_back((node.id, level + 1));
            }
            if omitted || more_neighbors {
                omitted = true;
                break;
            }
        }
        Ok(bounded(lines, VIEW_BUDGET, omitted))
    }

    pub fn changes(
        &mut self,
        changes: &WorktreeChanges,
        depth: u32,
        max_nodes: u32,
        cancelled: &AtomicBool,
    ) -> Result<String> {
        if depth > 3 || !(1..=50).contains(&max_nodes) {
            return Err("invalid changes parameters".into());
        }
        check_cancelled(cancelled)?;
        if changes.is_empty() {
            return Ok("no changes\n".into());
        }

        let mut lines = Vec::new();
        let mut line_bytes = 0;
        for record in &changes.records {
            let Some(line) = path_record_line(record) else {
                return Ok(bounded(lines, CHANGES_BUDGET, true));
            };
            if !push_change_line(&mut lines, &mut line_bytes, line) {
                return Ok(bounded(lines, CHANGES_BUDGET, true));
            }
        }
        if changes.files.is_empty() {
            return Ok(bounded(lines, CHANGES_BUDGET, false));
        }

        let tx = self.connection.transaction().map_err(db_error)?;
        let state = read_state(&tx)?;
        let root_limit = max_nodes as usize;
        let mut root_ids = Vec::with_capacity(root_limit);
        let mut omitted = false;
        let mut symbols = tx
            .prepare(
                "SELECT n.id, n.line_start, n.line_end
                   FROM files f JOIN nodes n ON n.file_id=f.id
                  WHERE f.path=?1 AND n.kind!='file'
                  ORDER BY n.line_start, n.line_end, n.id",
            )
            .map_err(db_error)?;

        let mut previous_path = None;
        for file in &changes.files {
            check_cancelled(cancelled)?;
            validate_changed_file(file)?;
            if previous_path.is_some_and(|path| path >= file.path.as_str()) {
                return Err("changed files are not uniquely path-sorted".into());
            }
            previous_path = Some(file.path.as_str());
            let mut span_index = 0;
            let mut coverage = Vec::new();
            let mut saw_symbol = false;
            let rows = symbols
                .query_map([&file.path], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                    ))
                })
                .map_err(db_error)?;
            for row in rows {
                check_cancelled(cancelled)?;
                let (id, line_start, line_end) = row.map_err(db_error)?;
                if id <= 0 || line_start == 0 || line_end < line_start {
                    return Err("database node interval is invalid".into());
                }
                saw_symbol = true;
                let interval = LineSpan {
                    start: u64::from(line_start) * 2,
                    end: u64::from(line_end) * 2,
                };
                merge_span(&mut coverage, interval);
                while file
                    .spans
                    .get(span_index)
                    .is_some_and(|span| span.end < interval.start)
                {
                    span_index += 1;
                }
                let changed = file.whole_file
                    || file
                        .spans
                        .get(span_index)
                        .is_some_and(|span| span.start <= interval.end);
                if changed {
                    if root_ids.len() < root_limit {
                        root_ids.push(id);
                    } else {
                        omitted = true;
                    }
                }
            }

            let unmapped = file.report_unmapped
                && if file.spans.is_empty() {
                    !file.whole_file || !saw_symbol
                } else {
                    has_unmapped_span(&file.spans, &coverage)
                };
            if unmapped {
                let Some(line) = path_line("changed ", &file.path) else {
                    return Ok(bounded(lines, CHANGES_BUDGET, true));
                };
                if !push_change_line(&mut lines, &mut line_bytes, line) {
                    return Ok(bounded(lines, CHANGES_BUDGET, true));
                }
            }
        }
        drop(symbols);

        let mut roots = Vec::with_capacity(root_ids.len());
        for id in root_ids {
            check_cancelled(cancelled)?;
            roots.push(load_node(&tx, id)?.ok_or_else(|| "changed node not found".to_owned())?);
        }
        for root in &roots {
            let Some(line) = root.line(&state, None, CHANGES_BUDGET)? else {
                omitted = true;
                break;
            };
            if !push_change_line(&mut lines, &mut line_bytes, line) {
                omitted = true;
                break;
            }
        }
        if !omitted && !roots.is_empty() {
            omitted = traverse_changes(
                &tx,
                &state,
                &roots,
                depth,
                root_limit,
                cancelled,
                (&mut lines, &mut line_bytes),
            )?;
        }
        if lines.is_empty() && !omitted {
            Ok("no changes\n".into())
        } else {
            Ok(bounded(lines, CHANGES_BUDGET, omitted))
        }
    }
}

impl NodeKind {
    fn db(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Type => "type",
            Self::Function => "function",
            Self::Test => "test",
        }
    }
}

impl RefKind {
    fn db(self) -> &'static str {
        match self {
            Self::Calls => "CALLS",
            Self::Imports => "IMPORTS",
        }
    }
}

impl EdgeKind {
    fn db(self) -> &'static str {
        match self {
            Self::Calls => "CALLS",
            Self::TestCalls => "TEST_CALLS",
            Self::Imports => "IMPORTS",
        }
    }
}

struct RowNode {
    id: i64,
    kind: String,
    name: String,
    path: String,
    line: u32,
}

impl RowNode {
    fn line(&self, state: &State, relation: Option<&str>, budget: usize) -> Result<Option<String>> {
        if self.id <= 0 {
            return Err("database node id is invalid".into());
        }
        let kind = title(&self.kind).ok_or_else(|| "database node kind is invalid".to_owned())?;
        let prefix = relation.map_or(String::new(), |value| format!("  {value} "));
        let mut output = format!(
            "{prefix}{}:{}:{} {kind} ",
            state.epoch, state.generation, self.id
        );
        if !push_escaped(&mut output, &self.name, budget)
            || !push_literal(&mut output, " ", budget)
            || !push_escaped(&mut output, &self.path, budget)
            || !push_literal(&mut output, &format!(":{}\n", self.line), budget)
        {
            return Ok(None);
        }
        Ok(Some(output))
    }
}

const CHANGE_NEIGHBORS: [(&str, &str); 4] = [
    (
        "test <-",
        "SELECT n.id, n.kind, n.name, f.path, n.line_start
           FROM edges e JOIN nodes n ON n.id=e.source_id
           JOIN files f ON f.id=n.file_id
          WHERE e.target_id=?1 AND e.kind='TEST_CALLS'
          ORDER BY e.source_id LIMIT ?2",
    ),
    (
        "caller <-",
        "SELECT n.id, n.kind, n.name, f.path, n.line_start
           FROM edges e JOIN nodes n ON n.id=e.source_id
           JOIN files f ON f.id=n.file_id
          WHERE e.target_id=?1 AND e.kind='CALLS'
          ORDER BY e.source_id LIMIT ?2",
    ),
    (
        "call ->",
        "SELECT n.id, n.kind, n.name, f.path, n.line_start
           FROM edges e JOIN nodes n ON n.id=e.target_id
           JOIN files f ON f.id=n.file_id
          WHERE e.source_id=?1 AND e.kind IN ('CALLS','TEST_CALLS')
          ORDER BY e.kind, e.target_id LIMIT ?2",
    ),
    (
        "import ->",
        "SELECT n.id, n.kind, n.name, f.path, n.line_start
           FROM edges e JOIN nodes n ON n.id=e.target_id
           JOIN files f ON f.id=n.file_id
          WHERE e.source_id=?1 AND e.kind='IMPORTS'
          ORDER BY e.target_id LIMIT ?2",
    ),
];

fn traverse_changes(
    connection: &Connection,
    state: &State,
    roots: &[RowNode],
    depth: u32,
    max_nodes: usize,
    cancelled: &AtomicBool,
    output: (&mut Vec<String>, &mut usize),
) -> Result<bool> {
    let (lines, line_bytes) = output;
    let mut visited = roots.iter().map(|node| node.id).collect::<HashSet<_>>();
    let mut current = roots.iter().map(|node| node.id).collect::<Vec<_>>();
    let mut next = Vec::with_capacity(max_nodes);
    let mut row_budget = max_nodes + 1;

    for level in 0..=depth {
        next.clear();
        for (relation, sql) in CHANGE_NEIGHBORS {
            let mut statement = connection.prepare(sql).map_err(db_error)?;
            for source in &current {
                check_cancelled(cancelled)?;
                let limit = row_budget.min(max_nodes.saturating_sub(visited.len()) + 1);
                if limit == 0 {
                    return Ok(true);
                }
                let limit = i64::try_from(limit)
                    .map_err(|_| "neighbor limit exceeds SQLite range".to_owned())?;
                let mut fetched = 0;
                let rows = statement
                    .query_map(params![source, limit], |row| {
                        Ok(RowNode {
                            id: row.get(0)?,
                            kind: row.get(1)?,
                            name: row.get(2)?,
                            path: row.get(3)?,
                            line: row.get(4)?,
                        })
                    })
                    .map_err(db_error)?;
                for row in rows {
                    check_cancelled(cancelled)?;
                    fetched += 1;
                    row_budget -= 1;
                    let node = row.map_err(db_error)?;
                    if visited.contains(&node.id) {
                        continue;
                    }
                    if level == depth || visited.len() == max_nodes {
                        return Ok(true);
                    }
                    let Some(line) = node.line(state, Some(relation), CHANGES_BUDGET)? else {
                        return Ok(true);
                    };
                    if !push_change_line(lines, line_bytes, line) {
                        return Ok(true);
                    }
                    visited.insert(node.id);
                    next.push(node.id);
                }
                if fetched == limit as usize {
                    return Ok(true);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        std::mem::swap(&mut current, &mut next);
    }
    Ok(false)
}

fn validate_changed_file(file: &ChangedFile) -> Result<()> {
    if file.path.is_empty()
        || file.spans.iter().any(|span| span.start > span.end)
        || file
            .spans
            .windows(2)
            .any(|spans| spans[0].end >= spans[1].start)
    {
        Err("invalid changed-file intervals".into())
    } else {
        Ok(())
    }
}

fn merge_span(spans: &mut Vec<LineSpan>, span: LineSpan) {
    if let Some(previous) = spans.last_mut()
        && span.start <= previous.end.saturating_add(1)
    {
        previous.end = previous.end.max(span.end);
    } else {
        spans.push(span);
    }
}

fn has_unmapped_span(changes: &[LineSpan], coverage: &[LineSpan]) -> bool {
    let mut covered = 0;
    changes.iter().any(|change| {
        while coverage
            .get(covered)
            .is_some_and(|symbol| symbol.end < change.start)
        {
            covered += 1;
        }
        coverage
            .get(covered)
            .is_none_or(|symbol| symbol.start > change.end)
    })
}

fn path_record_line(record: &PathRecord) -> Option<String> {
    match record {
        PathRecord::Deleted(path) => path_line("deleted ", path),
        PathRecord::Renamed(old, new) => {
            let mut output = String::from("renamed ");
            if !push_escaped(&mut output, old, CHANGES_BUDGET)
                || !push_literal(&mut output, " -> ", CHANGES_BUDGET)
                || !push_escaped(&mut output, new, CHANGES_BUDGET)
                || !push_literal(&mut output, "\n", CHANGES_BUDGET)
            {
                None
            } else {
                Some(output)
            }
        }
    }
}

fn path_line(prefix: &str, path: &str) -> Option<String> {
    let mut output = prefix.to_owned();
    if push_escaped(&mut output, path, CHANGES_BUDGET)
        && push_literal(&mut output, "\n", CHANGES_BUDGET)
    {
        Some(output)
    } else {
        None
    }
}

fn push_change_line(lines: &mut Vec<String>, bytes: &mut usize, line: String) -> bool {
    let Some(total) = bytes.checked_add(line.len()) else {
        return false;
    };
    if total > CHANGES_BUDGET {
        false
    } else {
        *bytes = total;
        lines.push(line);
        true
    }
}

fn load_stored_files(connection: &Connection) -> Result<HashMap<String, StoredFile>> {
    let mut statement = connection
        .prepare("SELECT id, path, git_oid, content_hash, parse_context, byte_size FROM files")
        .map_err(db_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(db_error)?;
    let mut files = HashMap::new();
    for row in rows {
        let (id, path, git_oid, hash, parse_context, byte_size) = row.map_err(db_error)?;
        if id <= 0 || byte_size < 0 || !git_oid.as_deref().is_none_or(valid_oid) {
            return Err("database file metadata is invalid".into());
        }
        let content_hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| "database content hash is invalid".to_owned())?;
        let byte_size =
            u64::try_from(byte_size).map_err(|_| "database file size is invalid".to_owned())?;
        if files
            .insert(
                path,
                StoredFile {
                    id,
                    git_oid,
                    content_hash,
                    parse_context,
                    byte_size,
                },
            )
            .is_some()
        {
            return Err("database contains duplicate file paths".into());
        }
    }
    Ok(files)
}

fn apply_incremental(
    tx: &Transaction<'_>,
    graph: &Graph,
    existing: &HashMap<String, StoredFile>,
    cancelled: &AtomicBool,
) -> Result<usize> {
    if !graph.edges.is_empty()
        || graph
            .refs
            .iter()
            .any(|reference| reference.resolved_target_key.is_some())
    {
        return Err("incremental graph contains resolved edges".into());
    }

    let mut current = HashMap::with_capacity(graph.files.len());
    for file in &graph.files {
        if !file.git_oid.as_deref().is_none_or(valid_oid)
            || current.insert(file.path.as_str(), file).is_some()
            || (!file.replace && !existing.contains_key(&file.path))
        {
            return Err("incremental file metadata is invalid".into());
        }
    }
    let mut removed = existing
        .iter()
        .filter(|(path, _)| current.get(path.as_str()).is_none_or(|file| file.replace))
        .map(|(_, file)| file.id)
        .collect::<Vec<_>>();
    removed.sort_unstable();
    let changed = removed.len()
        + graph
            .files
            .iter()
            .filter(|file| file.replace && !existing.contains_key(&file.path))
            .count();
    if changed == 0 && (!graph.nodes.is_empty() || !graph.refs.is_empty()) {
        return Err("no-op incremental graph contains parsed rows".into());
    }

    let metadata_changed = graph.files.iter().filter(|file| !file.replace).any(|file| {
        let old = existing
            .get(&file.path)
            .expect("validated existing file above");
        old.git_oid != file.git_oid
            || old.content_hash != file.content_hash
            || old.parse_context != file.parse_context
            || old.byte_size != file.byte_size
    });
    if metadata_changed {
        let mut update = tx
            .prepare(
                "UPDATE files
                    SET git_oid=?1, content_hash=?2, parse_context=?3, byte_size=?4
                  WHERE id=?5",
            )
            .map_err(db_error)?;
        for file in graph.files.iter().filter(|file| !file.replace) {
            check_cancelled(cancelled)?;
            let old = existing
                .get(&file.path)
                .ok_or_else(|| "incremental file is missing from the database".to_owned())?;
            if old.git_oid != file.git_oid
                || old.content_hash != file.content_hash
                || old.parse_context != file.parse_context
                || old.byte_size != file.byte_size
            {
                update
                    .execute(params![
                        file.git_oid,
                        file.content_hash.as_slice(),
                        file.parse_context,
                        i64::try_from(file.byte_size)
                            .map_err(|_| "file size exceeds SQLite range".to_owned())?,
                        old.id
                    ])
                    .map_err(db_error)?;
            }
        }
    }
    if changed == 0 {
        return Ok(0);
    }

    let mut affected_keys = HashSet::new();
    let mut affected_owners = HashSet::new();
    {
        let mut keys = tx
            .prepare(
                "SELECT nk.key FROM node_keys nk
                   JOIN nodes n ON n.id=nk.node_id
                  WHERE n.file_id=?1",
            )
            .map_err(db_error)?;
        let mut owners = tx
            .prepare("SELECT owner_key FROM nodes WHERE file_id=?1 AND owner_key IS NOT NULL")
            .map_err(db_error)?;
        for file_id in &removed {
            check_cancelled(cancelled)?;
            for row in keys
                .query_map([file_id], |row| row.get::<_, String>(0))
                .map_err(db_error)?
            {
                let key = row.map_err(db_error)?;
                if key.starts_with("rust:type:") {
                    affected_owners.insert(key.clone());
                }
                affected_keys.insert(key);
            }
            for row in owners
                .query_map([file_id], |row| row.get::<_, String>(0))
                .map_err(db_error)?
            {
                affected_owners.insert(row.map_err(db_error)?);
            }
        }
    }
    for node in &graph.nodes {
        for key in &node.keys {
            if key.starts_with("rust:type:") {
                affected_owners.insert(key.clone());
            }
            affected_keys.insert(key.clone());
        }
        if let Some(owner) = &node.owner_key {
            affected_owners.insert(owner.clone());
        }
    }

    let mut affected_refs = HashSet::new();
    {
        let mut refs = tx
            .prepare("SELECT ref_id FROM ref_keys WHERE key=?1 ORDER BY ref_id")
            .map_err(db_error)?;
        for key in &affected_keys {
            check_cancelled(cancelled)?;
            for row in refs
                .query_map([key], |row| row.get::<_, i64>(0))
                .map_err(db_error)?
            {
                affected_refs.insert(row.map_err(db_error)?);
            }
        }
    }

    {
        let mut delete_fts = tx
            .prepare("DELETE FROM nodes_fts WHERE rowid IN (SELECT id FROM nodes WHERE file_id=?1)")
            .map_err(db_error)?;
        let mut delete_file = tx
            .prepare("DELETE FROM files WHERE id=?1")
            .map_err(db_error)?;
        for file_id in &removed {
            check_cancelled(cancelled)?;
            delete_fts.execute([file_id]).map_err(db_error)?;
            delete_file.execute([file_id]).map_err(db_error)?;
        }
    }

    let new_refs = insert_graph(tx, graph, cancelled, true)?;
    affected_refs.extend(new_refs);
    resolve_references(tx, affected_refs, cancelled)?;
    reparent_methods(tx, affected_owners, cancelled)?;
    Ok(changed)
}

fn resolve_references(
    tx: &Transaction<'_>,
    references: HashSet<i64>,
    cancelled: &AtomicBool,
) -> Result<()> {
    if references.is_empty() {
        return Ok(());
    }
    let mut references = references.into_iter().collect::<Vec<_>>();
    references.sort_unstable();
    let mut load_ref = tx
        .prepare(
            "SELECT r.kind, n.kind, r.resolved_target_id
               FROM refs r JOIN nodes n ON n.id=r.source_id
              WHERE r.id=?1",
        )
        .map_err(db_error)?;
    let mut load_keys = tx
        .prepare("SELECT key FROM ref_keys WHERE ref_id=?1 ORDER BY rank")
        .map_err(db_error)?;
    let mut candidates = tx
        .prepare("SELECT node_id FROM node_keys WHERE key=?1 ORDER BY node_id LIMIT 2")
        .map_err(db_error)?;
    let mut update_ref = tx
        .prepare("UPDATE refs SET resolved_target_id=?1 WHERE id=?2")
        .map_err(db_error)?;
    let mut decrement = tx
        .prepare(
            "UPDATE edges SET support_count=support_count-1
              WHERE source_id=(SELECT source_id FROM refs WHERE id=?1)
                AND target_id=?2 AND kind=?3 AND support_count>1",
        )
        .map_err(db_error)?;
    let mut delete_edge = tx
        .prepare(
            "DELETE FROM edges
              WHERE source_id=(SELECT source_id FROM refs WHERE id=?1)
                AND target_id=?2 AND kind=?3 AND support_count=1",
        )
        .map_err(db_error)?;
    let mut increment = tx
        .prepare(
            "INSERT INTO edges(source_id, target_id, kind, support_count)
             SELECT source_id, ?2, ?3, 1 FROM refs WHERE id=?1
             ON CONFLICT(source_id, target_id, kind)
             DO UPDATE SET support_count=support_count+1",
        )
        .map_err(db_error)?;

    for reference_id in references {
        check_cancelled(cancelled)?;
        let Some((ref_kind, source_kind, old_target)) = load_ref
            .query_row([reference_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })
            .optional()
            .map_err(db_error)?
        else {
            continue;
        };
        let mut new_target = None;
        for row in load_keys
            .query_map([reference_id], |row| row.get::<_, String>(0))
            .map_err(db_error)?
        {
            match candidate(&mut candidates, &row.map_err(db_error)?)? {
                DbCandidate::Unique(target) => {
                    new_target = Some(target);
                    break;
                }
                DbCandidate::Ambiguous => break,
                DbCandidate::Missing => {}
            }
        }
        if old_target == new_target {
            continue;
        }
        let edge_kind = match (ref_kind.as_str(), source_kind.as_str()) {
            ("IMPORTS", _) => "IMPORTS",
            ("CALLS", "test") => "TEST_CALLS",
            ("CALLS", "file" | "type" | "function") => "CALLS",
            _ => return Err("database reference kind is invalid".into()),
        };
        if let Some(target) = old_target
            && decrement
                .execute(params![reference_id, target, edge_kind])
                .map_err(db_error)?
                == 0
        {
            delete_edge
                .execute(params![reference_id, target, edge_kind])
                .map_err(db_error)?;
        }
        update_ref
            .execute(params![new_target, reference_id])
            .map_err(db_error)?;
        if let Some(target) = new_target {
            increment
                .execute(params![reference_id, target, edge_kind])
                .map_err(db_error)?;
        }
    }
    Ok(())
}

fn reparent_methods(
    tx: &Transaction<'_>,
    owners: HashSet<String>,
    cancelled: &AtomicBool,
) -> Result<()> {
    if owners.is_empty() {
        return Ok(());
    }
    let mut owners = owners.into_iter().collect::<Vec<_>>();
    owners.sort();
    let mut candidates = tx
        .prepare("SELECT node_id FROM node_keys WHERE key=?1 ORDER BY node_id LIMIT 2")
        .map_err(db_error)?;
    let mut methods = tx
        .prepare(
            "SELECT n.id, file_node.id FROM nodes n
               JOIN nodes file_node
                 ON file_node.file_id=n.file_id AND file_node.kind='file'
              WHERE n.owner_key=?1 ORDER BY n.id",
        )
        .map_err(db_error)?;
    let mut update = tx
        .prepare("UPDATE nodes SET parent_id=?1 WHERE id=?2")
        .map_err(db_error)?;
    for owner in owners {
        check_cancelled(cancelled)?;
        let unique = match candidate(&mut candidates, &owner)? {
            DbCandidate::Unique(target) => Some(target),
            _ => None,
        };
        for row in methods
            .query_map([owner], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(db_error)?
        {
            let (method, file) = row.map_err(db_error)?;
            update
                .execute(params![unique.unwrap_or(file), method])
                .map_err(db_error)?;
        }
    }
    Ok(())
}

enum DbCandidate {
    Missing,
    Unique(i64),
    Ambiguous,
}

fn candidate(statement: &mut rusqlite::Statement<'_>, key: &str) -> Result<DbCandidate> {
    let mut rows = statement.query([key]).map_err(db_error)?;
    let Some(first) = rows.next().map_err(db_error)? else {
        return Ok(DbCandidate::Missing);
    };
    let node = first.get(0).map_err(db_error)?;
    if rows.next().map_err(db_error)?.is_some() {
        Ok(DbCandidate::Ambiguous)
    } else {
        Ok(DbCandidate::Unique(node))
    }
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn insert_graph(
    tx: &Transaction<'_>,
    graph: &Graph,
    cancelled: &AtomicBool,
    delta: bool,
) -> Result<Vec<i64>> {
    let file_count = if delta {
        graph.files.iter().filter(|file| file.replace).count()
    } else {
        graph.files.len()
    };
    let mut files = HashMap::with_capacity(file_count);
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO files(
                    path, language, git_oid, content_hash, parse_context, byte_size
                 ) VALUES(?1, 'rust', ?2, ?3, ?4, ?5)",
            )
            .map_err(db_error)?;
        for file in graph.files.iter().filter(|file| !delta || file.replace) {
            check_cancelled(cancelled)?;
            let byte_size = i64::try_from(file.byte_size)
                .map_err(|_| "file size exceeds SQLite range".to_owned())?;
            insert
                .execute(params![
                    file.path,
                    file.git_oid,
                    file.content_hash.as_slice(),
                    file.parse_context,
                    byte_size
                ])
                .map_err(db_error)?;
            files.insert(
                file.path.as_str(),
                (tx.last_insert_rowid(), file.path.as_str()),
            );
        }
    }

    let mut nodes = HashMap::with_capacity(graph.nodes.len());
    {
        let mut insert_node = tx
            .prepare(
                "INSERT INTO nodes(
                    file_id, kind, name, qualified_name, parent_id, owner_key,
                    line_start, line_end, signature
                 ) VALUES(?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8)",
            )
            .map_err(db_error)?;
        let mut insert_key = tx
            .prepare("INSERT INTO node_keys(key, node_id) VALUES(?1, ?2)")
            .map_err(db_error)?;
        let mut insert_fts = tx
            .prepare(
                "INSERT INTO nodes_fts(rowid, name, qualified_name, path, signature)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(db_error)?;
        for node in &graph.nodes {
            check_cancelled(cancelled)?;
            let (file_id, path) = files
                .get(node.file_key.as_str())
                .ok_or_else(|| "node references an unknown file".to_owned())?;
            insert_node
                .execute(params![
                    file_id,
                    node.kind.db(),
                    node.name,
                    node.qualified_name,
                    node.owner_key,
                    node.line_start,
                    node.line_end,
                    node.signature
                ])
                .map_err(db_error)?;
            let node_id = tx.last_insert_rowid();
            nodes.insert(node.key.as_str(), node_id);
            for key in &node.keys {
                insert_key
                    .execute(params![key, node_id])
                    .map_err(db_error)?;
            }
            insert_fts
                .execute(params![
                    node_id,
                    node.name,
                    node.qualified_name,
                    path,
                    node.signature
                ])
                .map_err(db_error)?;
        }
    }

    {
        let mut update_parent = tx
            .prepare("UPDATE nodes SET parent_id=?1 WHERE id=?2")
            .map_err(db_error)?;
        for node in &graph.nodes {
            check_cancelled(cancelled)?;
            if let Some(parent) = &node.parent_key {
                let node_id = lookup(&nodes, &node.key, "node")?;
                let parent_id = lookup(&nodes, parent, "parent")?;
                update_parent
                    .execute(params![parent_id, node_id])
                    .map_err(db_error)?;
            }
        }
    }

    let mut reference_ids = if delta {
        Vec::with_capacity(graph.refs.len())
    } else {
        Vec::new()
    };
    {
        let mut insert_ref = tx
            .prepare(
                "INSERT INTO refs(source_id, kind, line, resolved_target_id)
                 VALUES(?1, ?2, ?3, ?4)",
            )
            .map_err(db_error)?;
        let mut insert_ref_key = tx
            .prepare("INSERT INTO ref_keys(ref_id, rank, key) VALUES(?1, ?2, ?3)")
            .map_err(db_error)?;
        for reference in &graph.refs {
            check_cancelled(cancelled)?;
            let source_id = lookup(&nodes, &reference.source_key, "reference source")?;
            let target_id = reference
                .resolved_target_key
                .as_ref()
                .map(|key| lookup(&nodes, key, "reference target"))
                .transpose()?;
            insert_ref
                .execute(params![
                    source_id,
                    reference.kind.db(),
                    reference.line,
                    target_id
                ])
                .map_err(db_error)?;
            let reference_id = tx.last_insert_rowid();
            if delta {
                reference_ids.push(reference_id);
            }
            for (rank, key) in reference.keys.iter().enumerate() {
                let rank = i64::try_from(rank)
                    .map_err(|_| "reference rank exceeds SQLite range".to_owned())?;
                insert_ref_key
                    .execute(params![reference_id, rank, key])
                    .map_err(db_error)?;
            }
        }
    }

    {
        let mut insert_edge = tx
            .prepare(
                "INSERT INTO edges(source_id, target_id, kind, support_count)
                 VALUES(?1, ?2, ?3, ?4)",
            )
            .map_err(db_error)?;
        for edge in &graph.edges {
            check_cancelled(cancelled)?;
            insert_edge
                .execute(params![
                    lookup(&nodes, &edge.source_key, "edge source")?,
                    lookup(&nodes, &edge.target_key, "edge target")?,
                    edge.kind.db(),
                    edge.support_count
                ])
                .map_err(db_error)?;
        }
    }
    Ok(reference_ids)
}

fn create_schema(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS state(
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            epoch TEXT NOT NULL CHECK(length(epoch)=8),
            generation INTEGER NOT NULL CHECK(generation>=0)
         );
         INSERT OR IGNORE INTO state VALUES(1, lower(hex(randomblob(4))), 0);
         CREATE TABLE files(
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            language TEXT NOT NULL,
            git_oid TEXT,
            content_hash BLOB NOT NULL CHECK(length(content_hash)=32),
            parse_context TEXT NOT NULL,
            byte_size INTEGER NOT NULL CHECK(byte_size>=0)
         );
         CREATE TABLE nodes(
            id INTEGER PRIMARY KEY,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            kind TEXT NOT NULL CHECK(kind IN ('file','type','function','test')),
            name TEXT NOT NULL,
            qualified_name TEXT NOT NULL UNIQUE,
            parent_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
            owner_key TEXT,
            line_start INTEGER NOT NULL CHECK(line_start>0),
            line_end INTEGER NOT NULL CHECK(line_end>=line_start),
            signature TEXT NOT NULL
         );
         CREATE INDEX nodes_parent ON nodes(parent_id, kind, line_start, id);
         CREATE INDEX nodes_owner ON nodes(owner_key, id) WHERE owner_key IS NOT NULL;
         CREATE INDEX nodes_file_lines ON nodes(file_id, line_start, line_end);
         CREATE UNIQUE INDEX nodes_one_file ON nodes(file_id) WHERE kind='file';
         CREATE TABLE node_keys(
            key TEXT NOT NULL,
            node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            PRIMARY KEY(key, node_id)
         ) WITHOUT ROWID;
         CREATE INDEX node_keys_node ON node_keys(node_id);
         CREATE TABLE refs(
            id INTEGER PRIMARY KEY,
            source_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            kind TEXT NOT NULL CHECK(kind IN ('CALLS','IMPORTS')),
            line INTEGER NOT NULL CHECK(line>0),
            resolved_target_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL
         );
         CREATE INDEX refs_source_target
             ON refs(source_id, kind, resolved_target_id);
         CREATE INDEX refs_target_source
             ON refs(resolved_target_id, kind, source_id);
         CREATE TABLE ref_keys(
            ref_id INTEGER NOT NULL REFERENCES refs(id) ON DELETE CASCADE,
            rank INTEGER NOT NULL CHECK(rank>=0),
            key TEXT NOT NULL,
            PRIMARY KEY(ref_id, rank)
         ) WITHOUT ROWID;
         CREATE INDEX ref_keys_key ON ref_keys(key, ref_id, rank);
         CREATE TABLE edges(
            source_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            target_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            kind TEXT NOT NULL CHECK(kind IN ('CALLS','TEST_CALLS','IMPORTS')),
            support_count INTEGER NOT NULL CHECK(support_count > 0),
            PRIMARY KEY(source_id, target_id, kind)
         ) WITHOUT ROWID;
         CREATE INDEX edges_source ON edges(source_id, kind, target_id);
         CREATE INDEX edges_target ON edges(target_id, kind, source_id);
         CREATE VIRTUAL TABLE nodes_fts
             USING fts5(name, qualified_name, path, signature);
         PRAGMA user_version=2;",
    )
    .map_err(db_error)
}

fn drop_graph_schema(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DROP TABLE IF EXISTS nodes_fts;
         DROP TABLE IF EXISTS edges;
         DROP TABLE IF EXISTS ref_keys;
         DROP TABLE IF EXISTS refs;
         DROP TABLE IF EXISTS node_keys;
         DROP TABLE IF EXISTS nodes;
         DROP TABLE IF EXISTS files;",
    )
    .map_err(db_error)
}

fn read_state(connection: &Connection) -> Result<State> {
    validate_state(query_state(connection).map_err(db_error)?)
}

fn read_state_cancelled(connection: &Connection, cancelled: &AtomicBool) -> Result<State> {
    validate_state(retry_sqlite(cancelled, || query_state(connection))?)
}

fn query_state(connection: &Connection) -> rusqlite::Result<State> {
    connection.query_row(
        "SELECT epoch, generation FROM state WHERE singleton=1",
        [],
        |row| {
            Ok(State {
                epoch: row.get(0)?,
                generation: row.get(1)?,
            })
        },
    )
}

fn validate_state(state: State) -> Result<State> {
    if state.epoch.len() != 8
        || !state.epoch.bytes().all(|byte| byte.is_ascii_hexdigit())
        || state.generation < 0
    {
        Err("database state is invalid".into())
    } else {
        Ok(state)
    }
}

fn load_node(connection: &Connection, id: i64) -> Result<Option<RowNode>> {
    connection
        .query_row(
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
             FROM nodes n JOIN files f ON f.id=n.file_id WHERE n.id=?1",
            [id],
            |row| {
                Ok(RowNode {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    path: row.get(3)?,
                    line: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(db_error)
}

fn load_neighbors(
    connection: &Connection,
    id: i64,
    limit: usize,
    include_members: bool,
) -> Result<(Vec<(String, RowNode)>, bool)> {
    let queries = [
        (
            "member ->",
            true,
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
               FROM nodes n JOIN files f ON f.id=n.file_id
              WHERE n.parent_id=?1
              ORDER BY n.kind, n.line_start, n.id LIMIT ?2",
        ),
        (
            "test <-",
            false,
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
               FROM edges e JOIN nodes n ON n.id=e.source_id
               JOIN files f ON f.id=n.file_id
              WHERE e.target_id=?1 AND e.kind='TEST_CALLS'
              ORDER BY e.source_id LIMIT ?2",
        ),
        (
            "caller <-",
            false,
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
               FROM edges e JOIN nodes n ON n.id=e.source_id
               JOIN files f ON f.id=n.file_id
              WHERE e.target_id=?1 AND e.kind='CALLS'
              ORDER BY e.source_id LIMIT ?2",
        ),
        (
            "call ->",
            false,
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
               FROM edges e JOIN nodes n ON n.id=e.target_id
               JOIN files f ON f.id=n.file_id
              WHERE e.source_id=?1 AND e.kind IN ('CALLS','TEST_CALLS')
              ORDER BY e.kind, e.target_id LIMIT ?2",
        ),
        (
            "in <-",
            false,
            "SELECT coalesce(p.id, file_node.id),
                    coalesce(p.kind, file_node.kind),
                    coalesce(p.name, file_node.name),
                    coalesce(parent_file.path, f.path),
                    coalesce(p.line_start, file_node.line_start)
               FROM nodes n
               LEFT JOIN nodes p
                 ON p.id=n.parent_id AND p.kind IN ('file','type')
               LEFT JOIN files parent_file ON parent_file.id=p.file_id
               JOIN files f ON f.id=n.file_id
               JOIN nodes file_node
                 ON file_node.file_id=f.id AND file_node.kind='file'
              WHERE n.id=?1 AND n.kind!='file'
              ORDER BY file_node.id LIMIT ?2",
        ),
        (
            "import ->",
            false,
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
               FROM edges e JOIN nodes n ON n.id=e.target_id
               JOIN files f ON f.id=n.file_id
              WHERE e.source_id=?1 AND e.kind='IMPORTS'
              ORDER BY e.target_id LIMIT ?2",
        ),
    ];
    let mut neighbors = Vec::with_capacity(limit);

    for (relation, members_only, sql) in queries {
        if members_only && !include_members {
            continue;
        }
        let remaining = limit - neighbors.len();
        if remaining == 0 {
            break;
        }
        let remaining = i64::try_from(remaining)
            .map_err(|_| "neighbor limit exceeds SQLite range".to_owned())?;
        let mut statement = connection.prepare(sql).map_err(db_error)?;
        let rows = statement
            .query_map(params![id, remaining], |row| {
                Ok(RowNode {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    path: row.get(3)?,
                    line: row.get(4)?,
                })
            })
            .map_err(db_error)?;
        for row in rows {
            neighbors.push((relation.to_owned(), row.map_err(db_error)?));
        }
    }

    let more = neighbors.len() == limit;
    Ok((neighbors, more))
}

fn literal_fts(query: &str) -> Result<String> {
    let terms = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{term}\"*"))
        .collect::<Vec<_>>();
    if terms.is_empty() {
        Err("query must contain a letter, number, or underscore".into())
    } else {
        Ok(terms.join(" AND "))
    }
}

fn parse_ref(value: &str) -> Result<(&str, i64, i64)> {
    let mut parts = value.split(':');
    let epoch = parts.next().unwrap_or_default();
    let generation = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "invalid node_ref".to_owned())?;
    let id = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "invalid node_ref".to_owned())?;
    if epoch.len() != 8
        || !epoch.bytes().all(|byte| byte.is_ascii_hexdigit())
        || generation < 0
        || id <= 0
        || parts.next().is_some()
    {
        Err("invalid node_ref".into())
    } else {
        Ok((epoch, generation, id))
    }
}

fn bounded(lines: Vec<String>, budget: usize, mut omitted: bool) -> String {
    let mut output = String::new();
    for (index, line) in lines.iter().enumerate() {
        let more = omitted || index + 1 < lines.len();
        let reserve = usize::from(more) * TRUNCATED.len();
        if output.len() + line.len() + reserve > budget {
            omitted = true;
            break;
        }
        output.push_str(line);
    }
    if omitted {
        output.push_str(TRUNCATED);
    }
    output
}

fn push_escaped(output: &mut String, value: &str, budget: usize) -> bool {
    for character in value.chars() {
        if character.is_control() {
            for escaped in character.escape_default() {
                if !push_literal(output, escaped.encode_utf8(&mut [0; 4]), budget) {
                    return false;
                }
            }
        } else if !push_literal(output, character.encode_utf8(&mut [0; 4]), budget) {
            return false;
        }
    }
    true
}

fn push_literal(output: &mut String, value: &str, budget: usize) -> bool {
    if output.len() + value.len() > budget {
        false
    } else {
        output.push_str(value);
        true
    }
}

fn title(kind: &str) -> Option<&str> {
    match kind {
        "file" => Some("File"),
        "type" => Some("Type"),
        "function" => Some("Function"),
        "test" => Some("Test"),
        _ => None,
    }
}

fn lookup(map: &HashMap<&str, i64>, key: &str, label: &str) -> Result<i64> {
    map.get(key)
        .copied()
        .ok_or_else(|| format!("unknown {label}"))
}

fn verify_sqlite() -> Result<()> {
    if rusqlite::version_number() < 3_051_003 {
        Err(format!(
            "bundled SQLite {} is below 3.51.3",
            rusqlite::version()
        ))
    } else {
        Ok(())
    }
}

fn begin_immediate<'connection>(
    connection: &'connection Connection,
    cancelled: &AtomicBool,
) -> Result<Transaction<'connection>> {
    let started = Instant::now();
    loop {
        check_cancelled(cancelled)?;
        match Transaction::new_unchecked(connection, TransactionBehavior::Immediate) {
            Ok(transaction) => return Ok(transaction),
            Err(error) if is_busy(&error) && started.elapsed() < BUSY_LIMIT => {
                thread::sleep(BUSY_POLL);
            }
            Err(error) => return Err(db_error(error)),
        }
    }
}

fn configure_journal(connection: &Connection, cancelled: &AtomicBool) -> Result<()> {
    retry_sqlite(cancelled, || {
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )
    })
}

fn retry_sqlite<T>(
    cancelled: &AtomicBool,
    mut operation: impl FnMut() -> rusqlite::Result<T>,
) -> Result<T> {
    let started = Instant::now();
    loop {
        check_cancelled(cancelled)?;
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_busy(&error) && started.elapsed() < BUSY_LIMIT => {
                thread::sleep(BUSY_POLL);
            }
            Err(error) => return Err(db_error(error)),
        }
    }
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn db_error(error: rusqlite::Error) -> String {
    error.to_string()
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        Err("index cancelled".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_queries_do_not_expose_fts_syntax() {
        assert_eq!(
            literal_fts("dispatch OR *").unwrap(),
            "\"dispatch\"* AND \"OR\"*"
        );
        assert!(literal_fts("***").is_err());
    }

    #[test]
    fn output_is_bounded_at_lines() {
        let output = bounded(vec!["a\n".repeat(800), "b\n".into()], SEARCH_BUDGET, false);
        assert!(output.len() <= SEARCH_BUDGET);
        assert!(output.ends_with(TRUNCATED));
    }

    #[test]
    fn row_fields_are_whole_or_omitted() {
        let state = State {
            epoch: "0123abcd".into(),
            generation: 1,
        };
        let mut node = RowNode {
            id: 1,
            kind: "function".into(),
            name: "\u{1b}run".into(),
            path: "src/lib.rs".into(),
            line: 1,
        };
        assert!(
            node.line(&state, None, 100)
                .unwrap()
                .unwrap()
                .contains("\\u{1b}")
        );
        node.name = "é".repeat(100);
        assert!(node.line(&state, None, 20).unwrap().is_none());
        node.kind = "bogus".into();
        assert!(node.line(&state, None, 100).is_err());
    }

    #[test]
    fn compact_output_preserves_unicode_and_escapes_controls() {
        let mut output = String::new();
        assert!(push_escaped(&mut output, "café\n", 32));
        assert_eq!(output, "café\\n");
    }

    #[test]
    fn schema_mismatch_does_not_change_journal_mode() {
        let path = std::env::temp_dir().join(format!(
            "grapher-store-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA user_version=999;")
            .unwrap();
        drop(connection);

        assert!(Store::open(&path, false, &AtomicBool::new(false)).is_err());
        let connection = Connection::open(&path).unwrap();
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn full_node_queue_is_not_automatically_truncated() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
            rebuild: false,
        };
        let graph = Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
            }],
            nodes: vec![
                NodeInput {
                    key: "root".into(),
                    file_key: "src/lib.rs".into(),
                    kind: NodeKind::Function,
                    name: "root".into(),
                    qualified_name: "root@src/lib.rs:1".into(),
                    parent_key: None,
                    owner_key: None,
                    line_start: 1,
                    line_end: 1,
                    signature: "fn root()".into(),
                    keys: vec![],
                },
                NodeInput {
                    key: "child".into(),
                    file_key: "src/lib.rs".into(),
                    kind: NodeKind::Function,
                    name: "child".into(),
                    qualified_name: "child@src/lib.rs:2".into(),
                    parent_key: None,
                    owner_key: None,
                    line_start: 2,
                    line_end: 2,
                    signature: "fn child()".into(),
                    keys: vec![],
                },
            ],
            edges: vec![EdgeInput {
                source_key: "root".into(),
                target_key: "child".into(),
                kind: EdgeKind::Calls,
                support_count: 1,
            }],
            ..Graph::default()
        };
        let cancelled = AtomicBool::new(false);
        let (state, _, ()) = store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let output = store
            .view(&format!("{}:{}:1", state.epoch, state.generation), 3, 2)
            .unwrap();
        assert!(output.contains("call ->"));
        assert!(!output.contains(TRUNCATED.trim()));
    }

    #[test]
    fn changes_map_gaps_and_traverse_in_global_priority_order() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
            rebuild: false,
        };
        let names = ["root", "test", "caller", "callee", "imported"];
        let kinds = [
            NodeKind::Function,
            NodeKind::Test,
            NodeKind::Function,
            NodeKind::Function,
            NodeKind::Type,
        ];
        let graph = Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 64,
                replace: true,
            }],
            nodes: names
                .iter()
                .enumerate()
                .map(|(index, name)| NodeInput {
                    key: (*name).into(),
                    file_key: "src/lib.rs".into(),
                    kind: kinds[index],
                    name: (*name).into(),
                    qualified_name: (*name).into(),
                    parent_key: None,
                    owner_key: None,
                    line_start: if index == 0 { 2 } else { index as u32 + 7 },
                    line_end: if index == 0 { 6 } else { index as u32 + 7 },
                    signature: String::new(),
                    keys: vec![],
                })
                .collect(),
            edges: vec![
                EdgeInput {
                    source_key: "test".into(),
                    target_key: "root".into(),
                    kind: EdgeKind::TestCalls,
                    support_count: 1,
                },
                EdgeInput {
                    source_key: "caller".into(),
                    target_key: "root".into(),
                    kind: EdgeKind::Calls,
                    support_count: 1,
                },
                EdgeInput {
                    source_key: "root".into(),
                    target_key: "callee".into(),
                    kind: EdgeKind::Calls,
                    support_count: 1,
                },
                EdgeInput {
                    source_key: "root".into(),
                    target_key: "imported".into(),
                    kind: EdgeKind::Imports,
                    support_count: 1,
                },
            ],
            ..Graph::default()
        };
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let changes = WorktreeChanges {
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                whole_file: false,
                spans: vec![LineSpan { start: 7, end: 7 }],
                report_unmapped: true,
            }],
            records: vec![
                PathRecord::Deleted("old.rs".into()),
                PathRecord::Renamed("before.rs".into(), "after.rs".into()),
            ],
        };
        let output = store.changes(&changes, 1, 10, &cancelled).unwrap();
        let positions = [
            "deleted old.rs",
            "renamed before.rs -> after.rs",
            " root ",
            "test <-",
            "caller <-",
            "call ->",
            "import ->",
        ]
        .map(|part| output.find(part).unwrap());
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{output}"
        );
        assert!(!output.contains(TRUNCATED.trim()), "{output}");

        let depth_zero = store.changes(&changes, 0, 10, &cancelled).unwrap();
        assert!(depth_zero.contains(TRUNCATED.trim()), "{depth_zero}");

        let unmapped = WorktreeChanges {
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                whole_file: false,
                spans: vec![],
                report_unmapped: true,
            }],
            records: vec![],
        };
        assert_eq!(
            store.changes(&unmapped, 0, 10, &cancelled).unwrap(),
            "changed src/lib.rs\n"
        );
    }

    #[test]
    fn failed_replacement_preserves_the_committed_graph() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
            rebuild: false,
        };
        let cancelled = AtomicBool::new(false);
        let (before, _, ()) = store
            .index_with(&cancelled, |_full, _existing| {
                Ok((single_node_graph("old"), ()))
            })
            .unwrap();

        let mut invalid = single_node_graph("new");
        invalid.edges.push(EdgeInput {
            source_key: "new".into(),
            target_key: "missing".into(),
            kind: EdgeKind::Calls,
            support_count: 1,
        });
        assert!(
            store
                .index_with(&cancelled, |_full, _existing| Ok((invalid, ())))
                .is_err()
        );

        let mut invalid_delta = single_node_graph("new");
        invalid_delta.nodes[0].parent_key = Some("missing".into());
        assert!(
            store
                .index_with(&cancelled, |_full, _existing| Ok((invalid_delta, ())))
                .is_err()
        );

        let after = read_state(&store.connection).unwrap();
        assert_eq!(after.generation, before.generation);
        assert_eq!(
            store
                .connection
                .query_row("SELECT name FROM nodes", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "old"
        );
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn neighbor_queries_stop_at_the_shared_budget() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
            rebuild: false,
        };
        let mut graph = single_node_graph("root");
        for index in 0..100 {
            let key = format!("child-{index}");
            graph.nodes.push(NodeInput {
                key: key.clone(),
                file_key: "src/lib.rs".into(),
                kind: NodeKind::Function,
                name: key.clone(),
                qualified_name: key.clone(),
                parent_key: None,
                owner_key: None,
                line_start: 1,
                line_end: 1,
                signature: String::new(),
                keys: vec![],
            });
            graph.edges.push(EdgeInput {
                source_key: "root".into(),
                target_key: key,
                kind: EdgeKind::Calls,
                support_count: 1,
            });
        }
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let (neighbors, more) = load_neighbors(&store.connection, 1, 3, false).unwrap();
        assert_eq!(neighbors.len(), 3);
        assert!(more);
    }

    fn single_node_graph(name: &str) -> Graph {
        Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
            }],
            nodes: vec![NodeInput {
                key: name.into(),
                file_key: "src/lib.rs".into(),
                kind: NodeKind::Function,
                name: name.into(),
                qualified_name: name.into(),
                parent_key: None,
                owner_key: None,
                line_start: 1,
                line_end: 1,
                signature: String::new(),
                keys: vec![],
            }],
            ..Graph::default()
        }
    }
}
