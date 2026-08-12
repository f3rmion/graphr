use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::git::{
    ChangedFile, DependencyMode, Language, LineSpan, PathRecord, WorktreeChanges,
    dependency_package,
};

pub(crate) const SCHEMA_VERSION: i64 = 4;
const SEARCH_BUDGET: usize = 1536;
const VIEW_BUDGET: usize = 4096;
// ponytail: bound per-request root analysis; raise only with streamed/batched ranking.
const CHANGE_ANALYSIS_LIMIT: usize = 500;
// ponytail: inspect a bounded dependency tail after first-party neighbors;
// raise only if real call sites reach more than 256 vendored symbols at once.
const DEPENDENCY_NEIGHBOR_SCAN_LIMIT: usize = 256;
const FLOW_DEPTH: u32 = 15;
const FLOW_SCAN_LIMIT: usize = 500;
const FLOW_QUERY_LIMIT: usize = 5_000;
const TRUNCATED: &str = "[truncated]\n";
const BUSY_LIMIT: Duration = Duration::from_secs(5);
const BUSY_POLL: Duration = Duration::from_millis(5);
// ponytail: stable rowid order stops at the output budget; add BM25 only if
// measured relevance warrants ranking every match.
const SEARCH_SQL: &str = "SELECT n.id, n.kind, n.name, f.path, n.line_start
       FROM nodes_fts
       JOIN nodes n ON n.id=nodes_fts.rowid
       JOIN files f ON f.id=n.file_id
      WHERE nodes_fts MATCH ?1 AND (?2 IS NULL OR n.kind=?2)
      ORDER BY nodes_fts.rowid
      LIMIT ?3";
const SECURITY_KEYWORDS: [&str; 25] = [
    "auth",
    "login",
    "password",
    "token",
    "session",
    "crypt",
    "secret",
    "credential",
    "permission",
    "sql",
    "query",
    "execute",
    "connect",
    "socket",
    "request",
    "http",
    "sanitize",
    "validate",
    "encrypt",
    "decrypt",
    "hash",
    "sign",
    "verify",
    "admin",
    "privilege",
];

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
    pub language: Language,
    pub git_oid: Option<String>,
    pub content_hash: [u8; 32],
    pub parse_context: String,
    pub byte_size: u64,
    pub replace: bool,
}

pub struct StoredFile {
    id: i64,
    pub language: Language,
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
    pub alias_key: Option<String>,
    pub resolved_target_key: Option<String>,
}

pub struct TraitImplementationInput {
    pub file_key: String,
    pub line_start: u32,
    pub line_end: u32,
    pub implementor_key: String,
    pub trait_key: String,
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
    pub trait_implementations: Vec<TraitImplementationInput>,
    pub edges: Vec<EdgeInput>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct State {
    pub epoch: String,
    pub generation: i64,
}

pub struct Store {
    connection: Connection,
}

impl Store {
    pub(crate) fn open_private_image(path: &Path, cancelled: &AtomicBool) -> Result<Self> {
        Self::open_with_parent(path, cancelled, true)
    }

    fn open_with_parent(
        path: &Path,
        cancelled: &AtomicBool,
        descriptor_parent: bool,
    ) -> Result<Self> {
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
        if !metadata.is_dir()
            && !(descriptor_parent
                && parent.starts_with("/proc/self/fd")
                && fs::metadata(parent).is_ok_and(|metadata| metadata.is_dir()))
        {
            return Err("database directory is not a regular directory".into());
        }

        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if !descriptor_parent {
            flags |= OpenFlags::SQLITE_OPEN_NOFOLLOW;
        }
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
        if !matches!(version, 0 | SCHEMA_VERSION) {
            return Err("database schema mismatch".into());
        }
        configure_journal(&connection, cancelled)?;

        let store = Self { connection };
        match version {
            0 => {}
            SCHEMA_VERSION => read_state_cancelled(&store.connection, cancelled).map(|_| ())?,
            _ => unreachable!("schema mismatch returned above"),
        }
        Ok(store)
    }

    pub fn open_reader(path: &Path) -> Result<Self> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if !has_process_descriptor_boundary(path) {
            flags |= OpenFlags::SQLITE_OPEN_NOFOLLOW;
        }
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
            return Err("database schema mismatch".into());
        }
        Ok(Self { connection })
    }

    pub fn seal(self, cancelled: &AtomicBool) -> Result<State> {
        check_cancelled(cancelled)?;
        if !self.connection.is_autocommit() {
            return Err("database has uncommitted work".into());
        }
        let state = read_state_cancelled(&self.connection, cancelled)?;
        let (busy, _, _): (i64, i64, i64) = retry_sqlite(cancelled, || {
            self.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
        })?;
        if busy != 0 {
            return Err("database WAL checkpoint is busy".into());
        }
        retry_sqlite(cancelled, || {
            self.connection.execute_batch(
                "PRAGMA journal_mode=DELETE;
                 PRAGMA synchronous=FULL;",
            )
        })?;
        let mode: String = self
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(db_error)?;
        if mode != "delete" {
            return Err("database did not leave WAL mode".into());
        }
        require_integrity(&self.connection)?;
        check_cancelled(cancelled)?;
        let path = self
            .connection
            .path()
            .map(PathBuf::from)
            .ok_or_else(|| "database has no filesystem path".to_owned())?;
        self.connection
            .close()
            .map_err(|(_, error)| format!("cannot close database: {error}"))?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| format!("cannot open sealed database: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("cannot sync sealed database: {error}"))?;
        let wal = sqlite_sidecar(&path, "-wal");
        if fs::metadata(&wal).is_ok_and(|metadata| metadata.len() != 0) {
            return Err("sealed database still has WAL content".into());
        }
        Ok(state)
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
        if !new_schema && version != SCHEMA_VERSION {
            return Err("database schema mismatch".into());
        }
        if !new_schema {
            read_state(&tx)?;
        }
        let full = new_schema;
        let existing = if full {
            HashMap::new()
        } else {
            load_stored_files(&tx)?
        };
        let (graph, value) = build(full, &existing)?;
        check_cancelled(cancelled)?;

        let changed = if full {
            create_schema(&tx)?;
            let (_, implementations) = insert_graph(&tx, &graph, cancelled, false)?;
            resolve_trait_implementations(&tx, implementations.into_iter().collect(), cancelled)?;
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
        Ok((state, changed, value))
    }

    pub fn search(
        &mut self,
        snapshot_id: &str,
        query: &str,
        kind: Option<NodeKind>,
        limit: u32,
    ) -> Result<String> {
        if query.trim().is_empty() || query.len() > 256 || !(1..=20).contains(&limit) {
            return Err("invalid search parameters".into());
        }
        let fts = literal_fts(query)?;
        let tx = self.connection.transaction().map_err(db_error)?;
        let state = read_state(&tx)?;
        let mut statement = tx.prepare(SEARCH_SQL).map_err(db_error)?;
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
            let Some(line) = node.line(snapshot_id, &state, None, SEARCH_BUDGET)? else {
                omitted = true;
                break;
            };
            lines.push(line);
        }
        Ok(bounded(lines, SEARCH_BUDGET, omitted))
    }

    pub fn view(
        &mut self,
        snapshot_id: &str,
        node_ref: &str,
        depth: u32,
        max_nodes: u32,
    ) -> Result<String> {
        if depth > 6 || !(1..=50).contains(&max_nodes) || node_ref.len() > 116 {
            return Err("invalid view parameters".into());
        }
        let (embedded_snapshot, epoch, generation, root_id) = parse_ref(node_ref)?;
        if embedded_snapshot != snapshot_id {
            return Err("node_snapshot_mismatch".into());
        }
        let tx = self.connection.transaction().map_err(db_error)?;
        let state = read_state(&tx)?;
        if epoch != state.epoch || generation != state.generation {
            return Err("stale node_ref".into());
        }
        let root = load_node(&tx, root_id)?.ok_or_else(|| "node not found".to_owned())?;
        let Some(root_line) = root.line(snapshot_id, &state, None, VIEW_BUDGET)? else {
            return Ok(TRUNCATED.into());
        };
        let root_has_members = matches!(root.kind.as_str(), "file" | "type");
        let mut lines = vec![root_line];
        let mut visited = HashSet::from([root_id]);
        let mut queue = VecDeque::from([(root_id, root.kind == "type", 0_u32)]);
        let mut row_budget = max_nodes as usize + 1;
        let mut omitted = false;

        while let Some((current, include_traits, level)) = queue.pop_front() {
            if row_budget == 0 {
                omitted = true;
                break;
            }
            let (neighbors, more_neighbors) = load_neighbors(
                &tx,
                current,
                row_budget,
                current == root_id && root_has_members,
                include_traits,
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
                let include_traits = node.kind == "type";
                let Some(line) = node.line(snapshot_id, &state, Some(&relation), VIEW_BUDGET)?
                else {
                    omitted = true;
                    break;
                };
                lines.push(line);
                queue.push_back((node.id, include_traits, level + 1));
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
        snapshot_id: &str,
        changes: &WorktreeChanges,
        depth: u32,
        max_nodes: u32,
        dependency_mode: DependencyMode,
        cancelled: &AtomicBool,
    ) -> Result<String> {
        if depth > 6 || !(1..=50).contains(&max_nodes) {
            return Err("invalid changes parameters".into());
        }
        check_cancelled(cancelled)?;
        if changes.is_empty() && changes.files.is_empty() && changes.records.is_empty() {
            return Ok("no changes\n".into());
        }
        let deleted_paths_unanalyzed = changes
            .records
            .iter()
            .filter(|record| matches!(record, PathRecord::Deleted(_)))
            .filter(|record| {
                dependency_mode == DependencyMode::Full
                    || !matches!(record, PathRecord::Deleted(path) if dependency_package(path).is_some())
            })
            .count();
        if changes.files.is_empty() {
            let flow_accounting = if deleted_paths_unanalyzed == 0 {
                "flows_total=0"
            } else {
                "flows_discovered=0 flows_total=unknown"
            };
            let mut output = format!(
                "risk overall=0.0000 changed_symbols_total=0 changed_symbols_analyzed=0 changed_symbols_emitted=0 changed_symbols_omitted=0 {flow_accounting} static_test_path_gaps=0 analysis_complete={} analysis_roots_omitted=0 deleted_paths_unanalyzed={deleted_paths_unanalyzed} neighborhood_omitted=false unmapped_ranges=0 file_mapped_ranges=0 dependency_analysis={} {}\n",
                deleted_paths_unanalyzed == 0,
                dependency_analysis(dependency_mode),
                risk_metadata(None),
            );
            output.push_str(&coverage_diagnostics(
                0,
                false,
                false,
                false,
                0,
                deleted_paths_unanalyzed,
                depth,
            ));
            return Ok(output);
        }
        for file in &changes.files {
            validate_changed_file(file)?;
        }
        if changes
            .files
            .windows(2)
            .any(|files| files[0].path >= files[1].path)
        {
            return Err("changed files are not uniquely path-sorted".into());
        }

        let untracked = changes
            .records
            .iter()
            .filter_map(|record| match record {
                PathRecord::Untracked(path) => Some(path.as_str()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let files = changes
            .files
            .iter()
            .filter(|file| {
                dependency_mode == DependencyMode::Full || dependency_package(&file.path).is_none()
            })
            .filter(|file| !untracked.contains(file.path.as_str()))
            .chain(
                changes
                    .files
                    .iter()
                    .filter(|file| {
                        dependency_mode == DependencyMode::Full
                            || dependency_package(&file.path).is_none()
                    })
                    .filter(|file| untracked.contains(file.path.as_str())),
            );
        let mut unmapped_lines = Vec::new();
        let mut unmapped_range_count = 0_usize;
        let mut file_mapped_range_count = 0_usize;
        let tx = self.connection.transaction().map_err(db_error)?;
        let state = read_state(&tx)?;
        let mut symbol_root_ids = Vec::with_capacity(CHANGE_ANALYSIS_LIMIT);
        let mut file_root_ids = Vec::new();
        let mut root_seen = HashSet::new();
        let mut symbols = tx
            .prepare(
                "SELECT id, line_start, line_end FROM (
                    SELECT n.id, n.line_start, n.line_end
                      FROM files f JOIN nodes n ON n.file_id=f.id
                     WHERE f.path=?1 AND n.kind!='file'
                    UNION ALL
                    SELECT i.resolved_implementor_id, i.line_start, i.line_end
                      FROM files f JOIN trait_implementations i ON i.file_id=f.id
                     WHERE f.path=?1 AND i.resolved_implementor_id IS NOT NULL
                    UNION ALL
                    SELECT i.resolved_trait_id, i.line_start, i.line_end
                      FROM files f JOIN trait_implementations i ON i.file_id=f.id
                     WHERE f.path=?1 AND i.resolved_trait_id IS NOT NULL
                 ) ORDER BY line_start, line_end, id",
            )
            .map_err(db_error)?;
        let mut file_nodes = tx
            .prepare(
                "SELECT n.id, n.line_end
                   FROM files f JOIN nodes n ON n.file_id=f.id
                  WHERE f.path=?1 AND n.kind='file'",
            )
            .map_err(db_error)?;

        for file in files {
            check_cancelled(cancelled)?;
            let file_node = file_nodes
                .query_row([&file.path], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, u32>(1)?))
                })
                .optional()
                .map_err(db_error)?;
            if file_node.is_some_and(|(id, line_end)| id <= 0 || line_end == 0) {
                return Err("database file interval is invalid".into());
            }
            let mut span_index = 0;
            let mut coverage = Vec::new();
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
                    return Err("database change interval is invalid".into());
                }
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
                if changed && root_seen.insert(id) {
                    symbol_root_ids.push(id);
                }
            }

            let whole_span = file_node.map(|(_, line_end)| LineSpan {
                start: 2,
                end: u64::from(line_end) * 2,
            });
            let changed_spans = if file.whole_file && file.spans.is_empty() {
                whole_span
                    .as_ref()
                    .map_or(file.spans.as_slice(), std::slice::from_ref)
            } else {
                &file.spans
            };
            let residual = unmapped_spans(changed_spans, &coverage);
            let unmapped = file.report_unmapped
                && if changed_spans.is_empty() {
                    true
                } else {
                    !residual.is_empty()
                };
            if unmapped {
                let line = unmapped_line(file, &residual)
                    .ok_or_else(|| "unmapped line exceeds address space".to_owned())?;
                let ranges = residual.len().max(1);
                if let Some((id, _)) = file_node {
                    if root_seen.insert(id) {
                        file_root_ids.push(id);
                    }
                    unmapped_lines.push(line.replacen("unmapped ", "file-mapped ", 1));
                    file_mapped_range_count = file_mapped_range_count.saturating_add(ranges);
                } else {
                    unmapped_lines.push(line);
                    unmapped_range_count = unmapped_range_count.saturating_add(ranges);
                }
            }
        }
        drop(symbols);
        drop(file_nodes);

        let changed_symbols_total = symbol_root_ids.len();
        let analysis_roots_total = changed_symbols_total.saturating_add(file_root_ids.len());
        let mut analysis_ids = symbol_root_ids
            .iter()
            .take(CHANGE_ANALYSIS_LIMIT)
            .copied()
            .collect::<Vec<_>>();
        analysis_ids.extend(
            file_root_ids
                .iter()
                .take(CHANGE_ANALYSIS_LIMIT - analysis_ids.len())
                .copied(),
        );
        let analysis_roots_omitted = analysis_roots_total.saturating_sub(analysis_ids.len());
        let mut roots = load_nodes(&tx, &analysis_ids)?;
        let analysis = analyze_changed_roots(
            &tx,
            &roots,
            CHANGE_ANALYSIS_LIMIT,
            dependency_mode,
            cancelled,
        )?;
        roots.sort_by(|left, right| {
            analysis
                .risks
                .get(&right.id)
                .map_or(0, |risk| risk.score)
                .cmp(&analysis.risks.get(&left.id).map_or(0, |risk| risk.score))
                .then_with(|| left.id.cmp(&right.id))
        });
        let root_neighborhood_omitted = analysis_roots_omitted > 0;

        let changed_symbols_emitted = roots
            .iter()
            .filter(|root| analysis.risks.contains_key(&root.id))
            .count();
        let changed_symbols_omitted = changed_symbols_total.saturating_sub(changed_symbols_emitted);
        let mut lines = unmapped_lines;
        for root in &roots {
            let relation = analysis.risks.get(&root.id).map(|risk| {
                format!(
                    "risk {}{}",
                    score_text(risk.score),
                    if risk.test_gap {
                        " no-static-test-path"
                    } else if risk.indirect_test_covered {
                        " indirect-test-covered"
                    } else {
                        ""
                    }
                )
            });
            let line = root
                .line(snapshot_id, &state, relation.as_deref(), usize::MAX)?
                .ok_or_else(|| "changed root line exceeds address space".to_owned())?;
            lines.push(line);
        }
        let neighborhood_omitted = if root_neighborhood_omitted {
            true
        } else if roots.is_empty() {
            false
        } else {
            traverse_changes(
                &tx,
                snapshot_id,
                &state,
                &roots,
                (depth, CHANGE_ANALYSIS_LIMIT),
                dependency_mode,
                cancelled,
                &mut lines,
            )?
        };
        for flow in &analysis.flows {
            lines.push(flow_line(flow, dependency_mode)?);
        }
        let overall = analysis
            .risks
            .values()
            .map(|risk| risk.score)
            .max()
            .unwrap_or(0);
        let top_risk = analysis
            .risks
            .iter()
            .min_by_key(|(id, risk)| (std::cmp::Reverse(risk.score), **id))
            .map(|(_, risk)| risk);
        let static_test_path_gaps = analysis.risks.values().filter(|risk| risk.test_gap).count();
        let flow_incomplete =
            analysis.flow_omitted || analysis_roots_omitted > 0 || deleted_paths_unanalyzed > 0;
        let analysis_incomplete = flow_incomplete || analysis.test_mapping_omitted;
        let flow_accounting = if flow_incomplete {
            format!(
                "flows_discovered={} flows_total=unknown",
                analysis.flows.len()
            )
        } else {
            format!("flows_total={}", analysis.flows.len())
        };
        lines.insert(
            0,
            coverage_diagnostics(
                unmapped_range_count,
                neighborhood_omitted,
                analysis.flow_omitted,
                analysis.test_mapping_omitted,
                analysis_roots_omitted,
                deleted_paths_unanalyzed,
                depth,
            ),
        );
        lines.insert(
            0,
            format!(
                "risk overall={} changed_symbols_total={} changed_symbols_analyzed={} changed_symbols_emitted={} changed_symbols_omitted={} {} static_test_path_gaps={} analysis_complete={} analysis_roots_omitted={} deleted_paths_unanalyzed={} neighborhood_omitted={} unmapped_ranges={} file_mapped_ranges={} dependency_analysis={} {}\n",
                score_text(overall),
                changed_symbols_total,
                analysis.risks.len(),
                changed_symbols_emitted,
                changed_symbols_omitted,
                flow_accounting,
                static_test_path_gaps,
                !analysis_incomplete,
                analysis_roots_omitted,
                deleted_paths_unanalyzed,
                neighborhood_omitted,
                unmapped_range_count,
                file_mapped_range_count,
                dependency_analysis(dependency_mode),
                risk_metadata(top_risk),
            ),
        );
        Ok(lines.concat())
    }
}

pub fn validate_image(path: &Path) -> Result<State> {
    let descriptor_file = is_process_descriptor_directory(path);
    let metadata = if descriptor_file {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    }
    .map_err(|error| format!("cannot inspect database {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err("database image is not a regular file".into());
    }
    require_no_sidecars(path)?;
    let mut flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if !has_process_descriptor_boundary(path) {
        flags |= OpenFlags::SQLITE_OPEN_NOFOLLOW;
    }
    let connection = Connection::open_with_flags(path, flags)
        .map_err(|error| format!("cannot open database {}: {error}", path.display()))?;
    connection.busy_timeout(Duration::ZERO).map_err(db_error)?;
    verify_sqlite()?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    if version != SCHEMA_VERSION {
        return Err("database schema mismatch".into());
    }
    let mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(db_error)?;
    if mode != "delete" {
        return Err("database image is not in DELETE journal mode".into());
    }
    let state = read_state(&connection)?;
    require_integrity(&connection)?;
    require_no_sidecars(path)?;
    Ok(state)
}

fn is_process_descriptor_directory(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::RootDir))
        && components
            .next()
            .is_some_and(|part| part.as_os_str() == "proc")
        && components
            .next()
            .is_some_and(|part| part.as_os_str() == "self")
        && components
            .next()
            .is_some_and(|part| part.as_os_str() == "fd")
        && components
            .next()
            .and_then(|part| part.as_os_str().to_str())
            .is_some_and(|fd| !fd.is_empty() && fd.bytes().all(|byte| byte.is_ascii_digit()))
        && components.next().is_none()
}

fn has_process_descriptor_boundary(path: &Path) -> bool {
    is_process_descriptor_directory(path)
        || path.parent().is_some_and(is_process_descriptor_directory)
}

fn require_no_sidecars(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        match fs::symlink_metadata(sqlite_sidecar(path, suffix)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot inspect database sidecar: {error}")),
            Ok(_) => return Err("database image has a SQLite sidecar".into()),
        }
    }
    Ok(())
}

fn require_integrity(connection: &Connection) -> Result<()> {
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(db_error)?;
    if integrity == "ok" {
        Ok(())
    } else {
        Err("database integrity check failed".into())
    }
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
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

#[derive(Clone)]
struct FlowNode {
    id: i64,
    kind: String,
    name: String,
    qualified_name: String,
    path: String,
    line: u32,
}

struct AffectedFlow {
    entry: FlowNode,
    nodes: Vec<FlowNode>,
    parents: HashMap<i64, i64>,
    changed: Vec<i64>,
    depth: u32,
    file_count: usize,
    criticality: u32,
}

#[derive(Clone, Copy)]
struct NodeRisk {
    score: u32,
    flow_component: u32,
    test_component: u32,
    security_component: u32,
    caller_component: u32,
    test_node: bool,
    test_gap: bool,
    indirect_test_covered: bool,
}

type NodeRiskCounts = HashMap<i64, (u32, u32, bool)>;

#[derive(Default)]
struct ChangeAnalysis {
    risks: HashMap<i64, NodeRisk>,
    flows: Vec<AffectedFlow>,
    flow_omitted: bool,
    test_mapping_omitted: bool,
}

impl RowNode {
    fn line(
        &self,
        snapshot_id: &str,
        state: &State,
        relation: Option<&str>,
        budget: usize,
    ) -> Result<Option<String>> {
        if self.id <= 0 {
            return Err("database node id is invalid".into());
        }
        let kind = title(&self.kind).ok_or_else(|| "database node kind is invalid".to_owned())?;
        let prefix = relation.map_or(String::new(), |value| format!("  {value} "));
        let mut output = format!(
            "{prefix}n1:{snapshot_id}:{}:{}:{} {kind} ",
            state.epoch, state.generation, self.id,
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

const CHANGE_NEIGHBORS: [(&str, bool, &str); 6] = [
    (
        "test <-",
        false,
        "SELECT n.id, n.kind, n.name, f.path, n.line_start
           FROM edges e JOIN nodes n ON n.id=e.source_id
           JOIN files f ON f.id=n.file_id
          WHERE e.target_id=?1 AND e.kind='TEST_CALLS'
          ORDER BY f.path GLOB '.cargo/vendor/*/*', e.source_id LIMIT ?2",
    ),
    (
        "caller <-",
        false,
        "SELECT n.id, n.kind, n.name, f.path, n.line_start
           FROM edges e JOIN nodes n ON n.id=e.source_id
           JOIN files f ON f.id=n.file_id
          WHERE e.target_id=?1 AND e.kind='CALLS'
          ORDER BY f.path GLOB '.cargo/vendor/*/*', e.source_id LIMIT ?2",
    ),
    (
        "impl <-",
        true,
        "SELECT DISTINCT n.id, n.kind, n.name, f.path, n.line_start
           FROM trait_implementations i JOIN nodes n ON n.id=i.resolved_implementor_id
           JOIN files f ON f.id=n.file_id
          WHERE i.resolved_trait_id=?1
          ORDER BY f.path GLOB '.cargo/vendor/*/*', n.id LIMIT ?2",
    ),
    (
        "call ->",
        false,
        "SELECT n.id, n.kind, n.name, f.path, n.line_start
           FROM edges e JOIN nodes n ON n.id=e.target_id
           JOIN files f ON f.id=n.file_id
          WHERE e.source_id=?1 AND e.kind IN ('CALLS','TEST_CALLS')
          ORDER BY f.path GLOB '.cargo/vendor/*/*', e.kind, e.target_id LIMIT ?2",
    ),
    (
        "implements ->",
        true,
        "SELECT DISTINCT n.id, n.kind, n.name, f.path, n.line_start
           FROM trait_implementations i JOIN nodes n ON n.id=i.resolved_trait_id
           JOIN files f ON f.id=n.file_id
          WHERE i.resolved_implementor_id=?1
          ORDER BY f.path GLOB '.cargo/vendor/*/*', n.id LIMIT ?2",
    ),
    (
        "import ->",
        false,
        "SELECT n.id, n.kind, n.name, f.path, n.line_start
           FROM edges e JOIN nodes n ON n.id=e.target_id
           JOIN files f ON f.id=n.file_id
          WHERE e.source_id=?1 AND e.kind='IMPORTS'
          ORDER BY f.path GLOB '.cargo/vendor/*/*', e.target_id LIMIT ?2",
    ),
];

// Snapshot identity and traversal/output state are all required at this boundary.
#[allow(clippy::too_many_arguments)]
fn traverse_changes(
    connection: &Connection,
    snapshot_id: &str,
    state: &State,
    roots: &[RowNode],
    limits: (u32, usize),
    dependency_mode: DependencyMode,
    cancelled: &AtomicBool,
    lines: &mut Vec<String>,
) -> Result<bool> {
    let (depth, max_nodes) = limits;
    let mut visited = roots.iter().map(|node| node.id).collect::<HashSet<_>>();
    let node_limit = visited.len().saturating_add(max_nodes);
    let mut current = roots
        .iter()
        .map(|node| (node.id, node.kind == "type"))
        .collect::<Vec<_>>();
    let mut next = Vec::with_capacity(max_nodes);
    let mut row_budget = max_nodes + 1;
    let mut dependency_packages = HashSet::new();
    let mut dependency_omitted = false;

    for _ in 0..depth {
        next.clear();
        for (relation, types_only, sql) in CHANGE_NEIGHBORS {
            let mut statement = connection.prepare(sql).map_err(db_error)?;
            for &(source, is_type) in &current {
                if types_only && !is_type {
                    continue;
                }
                check_cancelled(cancelled)?;
                if row_budget == 0 {
                    return Ok(true);
                }
                let limit = if dependency_mode == DependencyMode::Boundary {
                    row_budget.saturating_add(DEPENDENCY_NEIGHBOR_SCAN_LIMIT)
                } else {
                    row_budget
                };
                let sql_limit = i64::try_from(limit)
                    .map_err(|_| "neighbor limit exceeds SQLite range".to_owned())?;
                let mut fetched = 0;
                let rows = statement
                    .query_map(params![source, sql_limit], |row| {
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
                    let node = row.map_err(db_error)?;
                    if dependency_mode == DependencyMode::Boundary
                        && let Some(package) = dependency_package(&node.path)
                    {
                        if dependency_packages.contains(package) {
                            continue;
                        }
                        if dependency_packages.len() == DEPENDENCY_NEIGHBOR_SCAN_LIMIT {
                            dependency_omitted = true;
                            continue;
                        }
                        dependency_packages.insert(package.to_owned());
                        lines.push(format!(
                            "  {relation} dependency-boundary package={package}\n"
                        ));
                        continue;
                    }
                    if visited.contains(&node.id) {
                        continue;
                    }
                    if row_budget == 0 || visited.len() >= node_limit {
                        return Ok(true);
                    }
                    row_budget -= 1;
                    let line = node
                        .line(snapshot_id, state, Some(relation), usize::MAX)?
                        .ok_or_else(|| "changed neighbor line exceeds address space".to_owned())?;
                    lines.push(line);
                    visited.insert(node.id);
                    next.push((node.id, node.kind == "type"));
                }
                if fetched == limit {
                    return Ok(true);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        std::mem::swap(&mut current, &mut next);
    }
    Ok(dependency_omitted)
}

fn analyze_changed_roots(
    connection: &Connection,
    roots: &[RowNode],
    max_flows: usize,
    dependency_mode: DependencyMode,
    cancelled: &AtomicBool,
) -> Result<ChangeAnalysis> {
    let risk_root_ids = roots
        .iter()
        .filter(|node| node.kind != "file")
        .map(|node| node.id)
        .collect::<Vec<_>>();
    let risk_root_count = risk_root_ids.len();
    let risk_nodes = load_flow_nodes(connection, &risk_root_ids)?;
    let risk_nodes = risk_nodes
        .into_iter()
        .map(|node| (node.id, node))
        .collect::<HashMap<_, _>>();
    let (flow_roots, mut flow_omitted) =
        changed_flow_roots(connection, roots, max_flows, &risk_nodes)?;
    let flow_root_ids = flow_roots
        .iter()
        .map(|node| node.id)
        .collect::<HashSet<_>>();
    if risk_root_count == 0 && flow_root_ids.is_empty() {
        return Ok(ChangeAnalysis::default());
    }

    // ponytail: a request-wide query ceiling keeps on-demand discovery simple;
    // load an adjacency snapshot only if measured coverage needs a higher cap.
    let mut query_budget = FLOW_QUERY_LIMIT;
    let (entries, entries_omitted) = affected_entries(
        connection,
        &flow_roots,
        max_flows,
        &mut query_budget,
        dependency_mode,
        cancelled,
    )?;
    flow_omitted |= entries_omitted;
    let mut flows = Vec::with_capacity(entries.len());
    for entry in entries {
        check_cancelled(cancelled)?;
        if query_budget == 0 {
            flow_omitted = true;
            break;
        }
        let (flow, truncated) = trace_flow(
            connection,
            entry,
            &flow_root_ids,
            &mut query_budget,
            dependency_mode,
            cancelled,
        )?;
        flow_omitted |= truncated;
        if let Some(flow) = flow {
            flows.push(flow);
        }
    }
    flows.sort_by(|left, right| {
        right
            .criticality
            .cmp(&left.criticality)
            .then_with(|| left.entry.id.cmp(&right.entry.id))
    });

    let (risk_counts, risk_counts_omitted) = node_risk_counts(connection, &risk_root_ids)?;
    let mut risks = HashMap::with_capacity(risk_root_count);
    for root in roots.iter().filter(|node| node.kind != "file") {
        check_cancelled(cancelled)?;
        let node = risk_nodes
            .get(&root.id)
            .ok_or_else(|| "flow node not found".to_owned())?;
        let flow_score = flows
            .iter()
            .filter(|flow| flow.changed.binary_search(&root.id).is_ok())
            .fold(0_u32, |score, flow| score.saturating_add(flow.criticality));
        let &(callers, tests, directly_tested) = risk_counts
            .get(&root.id)
            .ok_or_else(|| "node risk counts are missing".to_owned())?;
        let is_test = node.kind == "test";
        risks.insert(
            root.id,
            node_risk(
                flow_score,
                if is_test { 5 } else { tests },
                security_sensitive(&node.name, &node.qualified_name),
                callers,
                is_test,
                !is_test && tests == 0,
                !is_test && tests > 0 && !directly_tested,
            ),
        );
    }

    Ok(ChangeAnalysis {
        risks,
        flows,
        flow_omitted,
        test_mapping_omitted: risk_counts_omitted,
    })
}

fn changed_flow_roots(
    connection: &Connection,
    roots: &[RowNode],
    limit: usize,
    risk_nodes: &HashMap<i64, FlowNode>,
) -> Result<(Vec<FlowNode>, bool)> {
    let mut nodes = Vec::new();
    let mut seen = HashSet::new();
    let mut expanded_files = HashSet::new();
    let mut omitted = false;
    let mut file_nodes = connection
        .prepare(
            "SELECT n.id, n.kind, n.name, n.qualified_name, f.path, n.line_start
               FROM nodes changed JOIN nodes n ON n.file_id=changed.file_id
               JOIN files f ON f.id=n.file_id
              WHERE changed.id=?1 AND n.kind='function'
              ORDER BY n.id LIMIT ?2",
        )
        .map_err(db_error)?;
    let fetch = i64::try_from(limit.saturating_add(1))
        .map_err(|_| "flow root limit exceeds SQLite range")?;

    for root in roots {
        if root.kind == "function" {
            if seen.insert(root.id) {
                nodes.push(
                    risk_nodes
                        .get(&root.id)
                        .cloned()
                        .ok_or_else(|| "flow node not found".to_owned())?,
                );
            }
            continue;
        }
        if root.kind == "test" || !expanded_files.insert(root.path.as_str()) {
            continue;
        }
        let rows = file_nodes
            .query_map(params![root.id, fetch], flow_node)
            .map_err(db_error)?;
        for row in rows {
            let node = row.map_err(db_error)?;
            if !seen.insert(node.id) {
                continue;
            }
            if nodes.len() == limit {
                omitted = true;
                break;
            }
            nodes.push(node);
        }
    }
    Ok((nodes, omitted))
}

fn affected_entries(
    connection: &Connection,
    roots: &[FlowNode],
    limit: usize,
    query_budget: &mut usize,
    dependency_mode: DependencyMode,
    cancelled: &AtomicBool,
) -> Result<(Vec<FlowNode>, bool)> {
    let mut entries = Vec::new();
    let mut omitted = false;
    let mut visited = HashSet::new();
    let mut dependency_packages = HashSet::new();
    let mut first_party_visited = 0;
    let mut queue = VecDeque::new();

    for start in roots.iter().filter(|node| node.kind != "test").cloned() {
        if visited.insert(start.id) {
            if dependency_mode == DependencyMode::Boundary
                && let Some(package) = dependency_package(&start.path)
            {
                dependency_packages.insert(package.to_owned());
            } else {
                first_party_visited += 1;
            }
            queue.push_back((start, 0_u32));
        }
    }

    while let Some((current, depth)) = queue.pop_front() {
        check_cancelled(cancelled)?;
        if dependency_mode == DependencyMode::Boundary
            && dependency_package(&current.path).is_some()
        {
            continue;
        }
        if *query_budget == 0 {
            omitted = true;
            break;
        }
        *query_budget -= 1;
        let remaining = FLOW_SCAN_LIMIT.saturating_sub(first_party_visited);
        let (callers, more) = load_flow_neighbors(
            connection,
            current.id,
            true,
            remaining.max(1),
            dependency_mode,
        )?;
        omitted |= more;

        if current.kind == "function" && (callers.is_empty() || conventional_entry(&current.name)) {
            if entries.len() == limit {
                return Ok((entries, true));
            }
            entries.push(current.clone());
        }
        if depth == FLOW_DEPTH {
            omitted |= callers.iter().any(|node| !visited.contains(&node.id));
            continue;
        }

        for caller in callers {
            if visited.contains(&caller.id) {
                continue;
            }
            if dependency_mode == DependencyMode::Boundary
                && let Some(package) = dependency_package(&caller.path)
            {
                if dependency_packages.contains(package) {
                    continue;
                }
                if dependency_packages.len() == DEPENDENCY_NEIGHBOR_SCAN_LIMIT {
                    omitted = true;
                    continue;
                }
                dependency_packages.insert(package.to_owned());
            } else if first_party_visited == FLOW_SCAN_LIMIT {
                omitted = true;
                queue.clear();
                break;
            } else {
                first_party_visited += 1;
            }
            visited.insert(caller.id);
            queue.push_back((caller, depth + 1));
        }
    }
    Ok((entries, omitted))
}

fn trace_flow(
    connection: &Connection,
    entry: FlowNode,
    root_ids: &HashSet<i64>,
    query_budget: &mut usize,
    dependency_mode: DependencyMode,
    cancelled: &AtomicBool,
) -> Result<(Option<AffectedFlow>, bool)> {
    let mut nodes = vec![entry.clone()];
    let mut parents = HashMap::new();
    let mut visited = HashSet::from([entry.id]);
    let mut dependency_packages = dependency_package(&entry.path)
        .map(|package| HashSet::from([package.to_owned()]))
        .unwrap_or_default();
    let mut first_party_visited = usize::from(dependency_packages.is_empty());
    let mut queue = VecDeque::from([(entry.clone(), 0_u32)]);
    let mut depth_reached = 0;
    let mut omitted = false;

    while let Some((current, depth)) = queue.pop_front() {
        check_cancelled(cancelled)?;
        if dependency_mode == DependencyMode::Boundary
            && dependency_package(&current.path).is_some()
        {
            continue;
        }
        if *query_budget == 0 {
            omitted = true;
            break;
        }
        *query_budget -= 1;
        let remaining = FLOW_SCAN_LIMIT.saturating_sub(first_party_visited);
        let (callees, more) = load_flow_neighbors(
            connection,
            current.id,
            false,
            remaining.max(1),
            dependency_mode,
        )?;
        omitted |= more;
        if depth == FLOW_DEPTH {
            omitted |= callees.iter().any(|node| !visited.contains(&node.id));
            continue;
        }

        for callee in callees {
            if visited.contains(&callee.id) {
                continue;
            }
            if dependency_mode == DependencyMode::Boundary
                && let Some(package) = dependency_package(&callee.path)
            {
                if dependency_packages.contains(package) {
                    continue;
                }
                if dependency_packages.len() == DEPENDENCY_NEIGHBOR_SCAN_LIMIT {
                    omitted = true;
                    continue;
                }
                dependency_packages.insert(package.to_owned());
            } else if first_party_visited == FLOW_SCAN_LIMIT {
                omitted = true;
                queue.clear();
                break;
            } else {
                first_party_visited += 1;
            }
            visited.insert(callee.id);
            parents.insert(callee.id, current.id);
            depth_reached = depth_reached.max(depth + 1);
            nodes.push(callee.clone());
            queue.push_back((callee, depth + 1));
        }
    }

    let mut changed = nodes
        .iter()
        .filter_map(|node| root_ids.contains(&node.id).then_some(node.id))
        .collect::<Vec<_>>();
    changed.sort_unstable();
    if nodes.len() < 2 || changed.is_empty() {
        return Ok((None, omitted));
    }
    let (criticality, file_count) = score_flow(connection, &nodes, depth_reached, cancelled)?;
    Ok((
        Some(AffectedFlow {
            entry,
            nodes,
            parents,
            changed,
            depth: depth_reached,
            file_count,
            criticality,
        }),
        omitted,
    ))
}

fn load_flow_nodes(connection: &Connection, ids: &[i64]) -> Result<Vec<FlowNode>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (1..=ids.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT n.id, n.kind, n.name, n.qualified_name, f.path, n.line_start
           FROM nodes n JOIN files f ON f.id=n.file_id
          WHERE n.id IN ({placeholders})"
    );
    let mut statement = connection.prepare(&sql).map_err(db_error)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(ids), flow_node)
        .map_err(db_error)?;
    let mut nodes = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?
        .into_iter()
        .map(|node| (node.id, node))
        .collect::<HashMap<_, _>>();
    ids.iter()
        .map(|id| {
            nodes
                .remove(id)
                .ok_or_else(|| "flow node not found".to_owned())
        })
        .collect()
}

fn load_flow_neighbors(
    connection: &Connection,
    id: i64,
    incoming: bool,
    limit: usize,
    dependency_mode: DependencyMode,
) -> Result<(Vec<FlowNode>, bool)> {
    let (neighbor, source, kind) = if incoming {
        ("source_id", "target_id", " AND n.kind='function'")
    } else {
        ("target_id", "source_id", "")
    };
    let fetch = limit
        .checked_add(1)
        .ok_or_else(|| "flow neighbor limit overflow".to_owned())?;
    let fetch = i64::try_from(fetch).map_err(|_| "flow neighbor limit exceeds SQLite range")?;
    let path_filter = if dependency_mode == DependencyMode::Boundary {
        " AND f.path NOT GLOB '.cargo/vendor/*/*'"
    } else {
        ""
    };
    let sql = format!(
        "SELECT n.id, n.kind, n.name, n.qualified_name, f.path, n.line_start
           FROM edges e JOIN nodes n ON n.id=e.{neighbor}
           JOIN files f ON f.id=n.file_id
          WHERE e.{source}=?1 AND e.kind='CALLS'{kind}{path_filter}
          ORDER BY n.id LIMIT ?2"
    );
    let mut statement = connection.prepare(&sql).map_err(db_error)?;
    let mut nodes = statement
        .query_map(params![id, fetch], flow_node)
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    let more = nodes.len() > limit;
    nodes.truncate(limit);
    if dependency_mode == DependencyMode::Boundary {
        let dependency_fetch = i64::try_from(DEPENDENCY_NEIGHBOR_SCAN_LIMIT + 1)
            .map_err(|_| "dependency neighbor limit exceeds SQLite range")?;
        let sql = format!(
            "SELECT MIN(n.id), MIN(n.kind), MIN(n.name), MIN(n.qualified_name),
                    MIN(f.path), MIN(n.line_start)
               FROM edges e JOIN nodes n ON n.id=e.{neighbor}
               JOIN files f ON f.id=n.file_id
              WHERE e.{source}=?1 AND e.kind='CALLS'{kind}
                AND f.path GLOB '.cargo/vendor/*/*'
              GROUP BY substr(f.path, 15, instr(substr(f.path, 15), '/') - 1)
              ORDER BY MIN(n.id) LIMIT ?2"
        );
        let mut statement = connection.prepare(&sql).map_err(db_error)?;
        let dependencies = statement
            .query_map(params![id, dependency_fetch], flow_node)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        let dependencies_more = dependencies.len() > DEPENDENCY_NEIGHBOR_SCAN_LIMIT;
        nodes.extend(
            dependencies
                .into_iter()
                .take(DEPENDENCY_NEIGHBOR_SCAN_LIMIT),
        );
        return Ok((nodes, more || dependencies_more));
    }
    Ok((nodes, more))
}

fn flow_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<FlowNode> {
    Ok(FlowNode {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        qualified_name: row.get(3)?,
        path: row.get(4)?,
        line: row.get(5)?,
    })
}

fn score_flow(
    connection: &Connection,
    nodes: &[FlowNode],
    depth: u32,
    cancelled: &AtomicBool,
) -> Result<(u32, usize)> {
    if nodes.is_empty() {
        return Ok((0, 0));
    }
    let mut files = HashSet::new();
    let mut security_nodes = 0_usize;
    for node in nodes {
        check_cancelled(cancelled)?;
        files.insert(node.path.as_str());
        security_nodes += usize::from(security_sensitive(&node.name, &node.qualified_name));
    }
    let placeholders = (1..=nodes.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let evidence = format!(
        "SELECT
            (SELECT count(*) FROM refs
              WHERE kind='CALLS' AND resolved_target_id IS NULL
                AND source_id IN ({placeholders})),
            (SELECT count(DISTINCT target_id) FROM edges
              WHERE kind='TEST_CALLS' AND target_id IN ({placeholders}))"
    );
    let (external_calls, tested_nodes) = connection
        .query_row(
            &evidence,
            rusqlite::params_from_iter(nodes.iter().map(|node| node.id)),
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(db_error)?;
    let external_calls =
        usize::try_from(external_calls).map_err(|_| "external call count is invalid")?;
    let tested_nodes = usize::try_from(tested_nodes).map_err(|_| "tested node count is invalid")?;
    Ok((
        flow_criticality(
            nodes.len(),
            files.len(),
            external_calls,
            security_nodes,
            tested_nodes,
            depth,
        ),
        files.len(),
    ))
}

fn flow_criticality(
    node_count: usize,
    file_count: usize,
    external_calls: usize,
    security_nodes: usize,
    tested_nodes: usize,
    depth: u32,
) -> u32 {
    if node_count == 0 {
        return 0;
    }
    let file_spread = if file_count > 1 {
        ((file_count - 1) as f64 / 4.0).min(1.0)
    } else {
        0.0
    };
    let external = (external_calls as f64 / 5.0).min(1.0);
    let security = (security_nodes as f64 / node_count as f64).min(1.0);
    let direct_test_gap = 1.0 - (tested_nodes as f64 / node_count as f64).min(1.0);
    let depth = (f64::from(depth) / 10.0).min(1.0);
    ((file_spread * 0.30
        + external * 0.20
        + security * 0.25
        + direct_test_gap * 0.15
        + depth * 0.10)
        .clamp(0.0, 1.0)
        * 10_000.0)
        .round() as u32
}

fn node_risk_counts(connection: &Connection, ids: &[i64]) -> Result<(NodeRiskCounts, bool)> {
    if ids.is_empty() {
        return Ok((HashMap::new(), false));
    }
    let values = (1..=ids.len())
        .map(|index| format!("(?{index})"))
        .collect::<Vec<_>>()
        .join(",");
    let caller_limit = FLOW_QUERY_LIMIT
        .checked_add(1)
        .ok_or_else(|| "caller traversal limit overflow".to_owned())?;
    let sql = format!(
        "WITH RECURSIVE changed(id) AS (VALUES {values}),
         callers(changed_id, node_id) AS (
             SELECT id, id FROM changed
             UNION
             SELECT callers.changed_id AS changed_id, edges.source_id AS node_id
               FROM callers JOIN edges ON edges.target_id=callers.node_id
              WHERE edges.kind='CALLS'
              ORDER BY changed_id, node_id
              LIMIT {caller_limit}
         )
         SELECT changed.id,
                (SELECT count(*) FROM edges
                  WHERE target_id=changed.id AND kind='CALLS'),
                (SELECT count(DISTINCT test.source_id)
                   FROM callers
                   JOIN edges test ON test.target_id=callers.node_id
                                  AND test.kind='TEST_CALLS'
                  WHERE callers.changed_id=changed.id),
                EXISTS(SELECT 1 FROM edges
                        WHERE target_id=changed.id AND kind='TEST_CALLS'),
                (SELECT count(*) FROM callers) > {FLOW_QUERY_LIMIT}
           FROM changed"
    );
    let mut statement = connection.prepare(&sql).map_err(db_error)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(ids), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })
        .map_err(db_error)?;
    let mut counts = HashMap::with_capacity(ids.len());
    let mut omitted = false;
    for row in rows {
        let (id, callers, tests, directly_tested, callers_omitted) = row.map_err(db_error)?;
        omitted |= callers_omitted;
        counts.insert(
            id,
            (
                u32::try_from(callers).map_err(|_| "caller count is invalid")?,
                u32::try_from(tests).map_err(|_| "test count is invalid")?,
                directly_tested,
            ),
        );
    }
    Ok((counts, omitted))
}

fn node_risk(
    flow_score: u32,
    tests: u32,
    security: bool,
    callers: u32,
    test_node: bool,
    test_gap: bool,
    indirect_test_covered: bool,
) -> NodeRisk {
    let flow_component = flow_score.min(2_500);
    let test_component = 3_000_u32.saturating_sub(tests.min(5) * 500);
    let security_component = u32::from(security) * 2_000;
    let caller_component = callers.min(2) * 500;
    NodeRisk {
        score: flow_component + test_component + security_component + caller_component,
        flow_component,
        test_component,
        security_component,
        caller_component,
        test_node,
        test_gap,
        indirect_test_covered,
    }
}

fn security_sensitive(name: &str, qualified_name: &str) -> bool {
    let name = name.to_lowercase();
    let qualified_name = qualified_name.to_lowercase();
    SECURITY_KEYWORDS
        .iter()
        .any(|keyword| name.contains(keyword) || qualified_name.contains(keyword))
}

fn conventional_entry(name: &str) -> bool {
    // ponytail: the index does not retain decorators; add decorator metadata
    // when framework-wired handlers become a measured flow-coverage gap.
    matches!(
        name,
        "main"
            | "__main__"
            | "handler"
            | "handle"
            | "lambda_handler"
            | "upgrade"
            | "downgrade"
            | "lifespan"
            | "get_db"
            | "do_GET"
            | "do_POST"
            | "do_PUT"
            | "do_DELETE"
            | "do_PATCH"
            | "do_HEAD"
            | "do_OPTIONS"
            | "log_message"
    ) || name.starts_with("on_")
        || name.starts_with("handle_")
}

fn score_text(score: u32) -> String {
    format!("{}.{:04}", score / 10_000, score % 10_000)
}

fn risk_metadata(risk: Option<&NodeRisk>) -> String {
    let Some(risk) = risk else {
        return "risk_direction=higher-is-riskier risk_components=flow:0.0000,test_paths:0.0000,security:0.0000,callers:0.0000 risk_rationale=no-symbol-risk test_path_confidence=heuristic test_path_provenance=resolved-static-call-graph".into();
    };
    let mut rationale = if risk.test_node {
        "changed-test".to_owned()
    } else if risk.test_gap {
        "no-static-test-path".to_owned()
    } else if risk.indirect_test_covered {
        "indirect-test-coverage".to_owned()
    } else {
        "direct-test-coverage".to_owned()
    };
    for (present, label) in [
        (risk.flow_component > 0, "affected-flow"),
        (risk.security_component > 0, "security-sensitive"),
        (risk.caller_component > 0, "caller-impact"),
    ] {
        if present {
            rationale.push('+');
            rationale.push_str(label);
        }
    }
    format!(
        "risk_direction=higher-is-riskier risk_components=flow:{},test_paths:{},security:{},callers:{} risk_rationale={rationale} test_path_confidence=heuristic test_path_provenance=resolved-static-call-graph",
        score_text(risk.flow_component),
        score_text(risk.test_component),
        score_text(risk.security_component),
        score_text(risk.caller_component),
    )
}

fn coverage_diagnostics(
    unmapped_ranges: usize,
    neighborhood_omitted: bool,
    flow_analysis_omitted: bool,
    test_mapping_omitted: bool,
    analysis_roots_omitted: usize,
    deleted_paths_unanalyzed: usize,
    depth: u32,
) -> String {
    let mut output = String::new();
    if unmapped_ranges > 0 {
        output.push_str(&format!(
            "coverage category=mapping status=incomplete items={unmapped_ranges} remediation=call-index-then-restart-changes\n"
        ));
    }
    if neighborhood_omitted {
        output.push_str(&format!(
            "coverage category=neighborhood status=incomplete items=unknown remediation=call-view-on-each-emitted-changed-node-ref depth={depth} max_nodes=50\n"
        ));
    }
    if flow_analysis_omitted {
        output.push_str(
            "coverage category=flow-analysis status=incomplete items=unknown remediation=narrow-review-base-and-restart-changes\n",
        );
    }
    if test_mapping_omitted {
        output.push_str(
            "coverage category=test-mapping status=incomplete items=unknown remediation=narrow-review-base-and-restart-changes\n",
        );
    }
    if analysis_roots_omitted > 0 {
        output.push_str(&format!(
            "coverage category=analysis-roots status=incomplete items={analysis_roots_omitted} remediation=narrow-review-base-and-restart-changes\n"
        ));
    }
    if deleted_paths_unanalyzed > 0 {
        output.push_str(&format!(
            "coverage category=deleted-paths status=incomplete items={deleted_paths_unanalyzed} remediation=review-corresponding-diff-pages\n"
        ));
    }
    if output.is_empty() {
        output.push_str("coverage status=complete remediation=none\n");
    }
    output
}

const fn dependency_analysis(mode: DependencyMode) -> &'static str {
    match mode {
        DependencyMode::Boundary => "collapsed",
        DependencyMode::Full => "full",
    }
}

fn flow_line(flow: &AffectedFlow, dependency_mode: DependencyMode) -> Result<String> {
    let changed = flow
        .nodes
        .iter()
        .rev()
        .find(|node| flow.changed.binary_search(&node.id).is_ok())
        .map(|node| node.id)
        .ok_or_else(|| "affected flow has no changed node".to_owned())?;
    let mut route = vec![changed];
    while route.last().copied() != Some(flow.entry.id) {
        let current = *route.last().expect("route starts with changed node");
        let parent = flow
            .parents
            .get(&current)
            .copied()
            .ok_or_else(|| "affected flow path is incomplete".to_owned())?;
        if route.contains(&parent) || route.len() > FLOW_DEPTH as usize {
            return Err("affected flow path is cyclic".into());
        }
        route.push(parent);
    }
    route.reverse();
    if dependency_mode == DependencyMode::Boundary {
        let mut best_tail: Option<Vec<i64>> = None;
        for boundary in flow
            .nodes
            .iter()
            .filter(|node| dependency_package(&node.path).is_some())
        {
            let mut tail = vec![boundary.id];
            let mut current = boundary.id;
            while current != changed {
                let Some(parent) = flow.parents.get(&current).copied() else {
                    tail.clear();
                    break;
                };
                if tail.contains(&parent) || tail.len() > FLOW_DEPTH as usize {
                    return Err("affected flow path is cyclic".into());
                }
                tail.push(parent);
                current = parent;
            }
            if tail.is_empty() {
                continue;
            }
            tail.reverse();
            tail.remove(0);
            if best_tail
                .as_ref()
                .is_none_or(|best| (tail.len(), &tail) < (best.len(), best))
            {
                best_tail = Some(tail);
            }
        }
        if let Some(tail) = best_tail {
            route.extend(tail);
        }
    }

    let mut output = format!(
        "flow {} depth={} nodes={} files={} changed={} ",
        score_text(flow.criticality),
        flow.depth,
        flow.nodes.len(),
        flow.file_count,
        flow.changed.len()
    );
    for (index, id) in route.into_iter().enumerate() {
        let node = flow
            .nodes
            .iter()
            .find(|node| node.id == id)
            .ok_or_else(|| "affected flow node is missing".to_owned())?;
        let mut step = String::new();
        let complete = if dependency_mode == DependencyMode::Boundary
            && let Some(package) = dependency_package(&node.path)
        {
            push_literal(
                &mut step,
                &format!("dependency-boundary[{package}]"),
                usize::MAX,
            )
        } else {
            push_escaped(&mut step, &node.name, usize::MAX)
                && push_literal(&mut step, "@", usize::MAX)
                && push_escaped(&mut step, &node.path, usize::MAX)
                && push_literal(&mut step, &format!(":{}", node.line), usize::MAX)
        };
        if !complete {
            return Err("affected flow line exceeds address space".into());
        }
        let separator = if index == 0 { "" } else { " -> " };
        output.push_str(separator);
        output.push_str(&step);
    }
    output.push('\n');
    Ok(output)
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

fn unmapped_spans(changes: &[LineSpan], coverage: &[LineSpan]) -> Vec<LineSpan> {
    let mut residual = Vec::new();
    let mut covered = 0;
    for change in changes {
        while coverage
            .get(covered)
            .is_some_and(|symbol| symbol.end < change.start)
        {
            covered += 1;
        }
        let deletion_anchor = change.start == change.end && change.start % 2 == 1;
        let mut cursor = Some(change.start);
        let mut index = covered;
        while let (Some(start), Some(symbol)) = (cursor, coverage.get(index)) {
            if symbol.start > change.end {
                break;
            }
            if start < symbol.start {
                push_unmapped_span(
                    &mut residual,
                    LineSpan {
                        start,
                        end: change.end.min(symbol.start - 1),
                    },
                    deletion_anchor,
                );
            }
            if symbol.end >= change.end {
                cursor = None;
                break;
            }
            cursor = Some(start.max(symbol.end + 1));
            index += 1;
        }
        if let Some(start) = cursor
            && start <= change.end
        {
            push_unmapped_span(
                &mut residual,
                LineSpan {
                    start,
                    end: change.end,
                },
                deletion_anchor,
            );
        }
        covered = index;
    }
    residual
}

fn push_unmapped_span(residual: &mut Vec<LineSpan>, span: LineSpan, deletion_anchor: bool) {
    if deletion_anchor {
        residual.push(span);
        return;
    }
    let Some(start) = span.start.checked_add(span.start % 2) else {
        return;
    };
    let end = span.end - span.end % 2;
    if start <= end {
        residual.push(LineSpan { start, end });
    }
}

fn unmapped_line(file: &ChangedFile, residual: &[LineSpan]) -> Option<String> {
    let mut output = String::from("unmapped ");
    if !push_escaped(&mut output, &file.path, usize::MAX) {
        return None;
    }
    let mut locations = 0;
    for span in residual {
        let start = (span.start / 2).max(1);
        let end = (span.end / 2).max(start);
        let location = if start == end {
            format!("{}{start}", if locations == 0 { ':' } else { ',' })
        } else {
            format!("{}{start}-{end}", if locations == 0 { ':' } else { ',' })
        };
        if !push_literal(&mut output, &location, usize::MAX) {
            return None;
        }
        locations += 1;
    }
    if locations == 0 && !push_literal(&mut output, ":1", usize::MAX) {
        return None;
    }
    push_literal(&mut output, "\n", usize::MAX).then_some(output)
}

fn load_stored_files(connection: &Connection) -> Result<HashMap<String, StoredFile>> {
    let mut statement = connection
        .prepare(
            "SELECT id, path, language, git_oid, content_hash, parse_context, byte_size FROM files",
        )
        .map_err(db_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(db_error)?;
    let mut files = HashMap::new();
    for row in rows {
        let (id, path, language, git_oid, hash, parse_context, byte_size) =
            row.map_err(db_error)?;
        if id <= 0 || byte_size < 0 || !git_oid.as_deref().is_none_or(valid_oid) {
            return Err("database file metadata is invalid".into());
        }
        let language = Language::parse(&language)
            .ok_or_else(|| "database file language is invalid".to_owned())?;
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
                    language,
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
            || (!file.replace
                && existing
                    .get(&file.path)
                    .is_some_and(|old| old.language != file.language))
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
    if changed == 0
        && (!graph.nodes.is_empty()
            || !graph.refs.is_empty()
            || !graph.trait_implementations.is_empty())
    {
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
        let mut aliases = tx
            .prepare(
                "SELECT r.alias_key FROM refs r
                   JOIN nodes n ON n.id=r.source_id
                  WHERE n.file_id=?1 AND r.alias_key IS NOT NULL",
            )
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
            for row in aliases
                .query_map([file_id], |row| row.get::<_, String>(0))
                .map_err(db_error)?
            {
                affected_keys.insert(row.map_err(db_error)?);
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
    for reference in &graph.refs {
        if let Some(alias) = &reference.alias_key {
            affected_keys.insert(alias.clone());
        }
    }

    let mut affected_refs = HashSet::new();
    let mut affected_implementations = HashSet::new();
    let mut affected_aliases = HashSet::new();
    {
        let mut refs = tx
            .prepare(
                "SELECT rk.ref_id, r.alias_key FROM ref_keys rk
                   JOIN refs r ON r.id=rk.ref_id
                  WHERE rk.key=?1 ORDER BY rk.ref_id",
            )
            .map_err(db_error)?;
        let mut implementations = tx
            .prepare(
                "SELECT id FROM trait_implementations
                  WHERE implementor_key=?1 OR trait_key=?1 ORDER BY id",
            )
            .map_err(db_error)?;
        for key in &affected_keys {
            check_cancelled(cancelled)?;
            for row in refs
                .query_map([key], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(db_error)?
            {
                let (reference, alias) = row.map_err(db_error)?;
                affected_refs.insert(reference);
                affected_aliases.extend(alias);
            }
            for row in implementations
                .query_map([key], |row| row.get::<_, i64>(0))
                .map_err(db_error)?
            {
                affected_implementations.insert(row.map_err(db_error)?);
            }
        }
        for alias in affected_aliases {
            for row in implementations
                .query_map([alias], |row| row.get::<_, i64>(0))
                .map_err(db_error)?
            {
                affected_implementations.insert(row.map_err(db_error)?);
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

    let (new_refs, new_implementations) = insert_graph(tx, graph, cancelled, true)?;
    affected_refs.extend(new_refs);
    affected_implementations.extend(new_implementations);
    resolve_references(tx, affected_refs, cancelled)?;
    resolve_trait_implementations(tx, affected_implementations, cancelled)?;
    reparent_methods(tx, affected_owners, cancelled)?;
    Ok(changed)
}

fn resolve_references(
    tx: &Transaction<'_>,
    mut references: HashSet<i64>,
    cancelled: &AtomicBool,
) -> Result<()> {
    if references.is_empty() {
        return Ok(());
    }
    let mut load_alias = tx
        .prepare("SELECT alias_key FROM refs WHERE id=?1")
        .map_err(db_error)?;
    let mut alias_keys = HashSet::new();
    let mut exporters = HashSet::new();
    for reference_id in &references {
        if let Some(alias) = load_alias
            .query_row([reference_id], |row| row.get::<_, Option<String>>(0))
            .optional()
            .map_err(db_error)?
            .flatten()
        {
            alias_keys.insert(alias);
            exporters.insert(*reference_id);
        }
    }
    let mut consumers = tx
        .prepare(
            "SELECT rk.ref_id, r.alias_key IS NOT NULL
               FROM ref_keys rk JOIN refs r ON r.id=rk.ref_id
              WHERE rk.key=?1 ORDER BY rk.ref_id",
        )
        .map_err(db_error)?;
    for alias in alias_keys {
        check_cancelled(cancelled)?;
        for row in consumers
            .query_map([alias], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))
            })
            .map_err(db_error)?
        {
            let (reference_id, exports) = row.map_err(db_error)?;
            references.insert(reference_id);
            if exports {
                exporters.insert(reference_id);
            }
        }
    }
    let mut references = references
        .into_iter()
        .map(|reference_id| (!exporters.contains(&reference_id), reference_id))
        .collect::<Vec<_>>();
    references.sort_unstable();
    let mut load_ref = tx
        .prepare(
            "SELECT r.kind, n.kind, r.alias_key, r.resolved_target_id
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
    let mut alias_candidates = tx
        .prepare(
            "SELECT count(*), count(resolved_target_id),
                    count(DISTINCT resolved_target_id), min(resolved_target_id)
               FROM refs WHERE alias_key=?1",
        )
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

    for (_, reference_id) in references {
        check_cancelled(cancelled)?;
        let Some((ref_kind, source_kind, alias_key, old_target)) = load_ref
            .query_row([reference_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
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
            let key = row.map_err(db_error)?;
            let direct = candidate(&mut candidates, &key)?;
            let alias = if alias_key.is_none() {
                alias_candidate(&mut alias_candidates, &key)?
            } else {
                DbCandidate::Missing
            };
            match merge_candidates(direct, alias) {
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

fn resolve_trait_implementations(
    tx: &Transaction<'_>,
    implementations: HashSet<i64>,
    cancelled: &AtomicBool,
) -> Result<()> {
    if implementations.is_empty() {
        return Ok(());
    }
    let mut implementations = implementations.into_iter().collect::<Vec<_>>();
    implementations.sort_unstable();
    let mut load = tx
        .prepare(
            "SELECT implementor_key, trait_key,
                    resolved_implementor_id, resolved_trait_id
               FROM trait_implementations WHERE id=?1",
        )
        .map_err(db_error)?;
    let mut candidates = tx
        .prepare(
            "SELECT nk.node_id FROM node_keys nk JOIN nodes n ON n.id=nk.node_id
              WHERE nk.key=?1 AND n.kind='type' ORDER BY nk.node_id LIMIT 2",
        )
        .map_err(db_error)?;
    let mut alias_candidates = tx
        .prepare(
            "SELECT count(*), count(r.resolved_target_id),
                    count(DISTINCT CASE WHEN n.kind='type' THEN r.resolved_target_id END),
                    min(CASE WHEN n.kind='type' THEN r.resolved_target_id END)
               FROM refs r LEFT JOIN nodes n ON n.id=r.resolved_target_id
              WHERE r.alias_key=?1",
        )
        .map_err(db_error)?;
    let mut update = tx
        .prepare(
            "UPDATE trait_implementations
                SET resolved_implementor_id=?1, resolved_trait_id=?2
              WHERE id=?3",
        )
        .map_err(db_error)?;

    for implementation_id in implementations {
        check_cancelled(cancelled)?;
        let Some((implementor_key, trait_key, old_implementor, old_trait)) = load
            .query_row([implementation_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })
            .optional()
            .map_err(db_error)?
        else {
            continue;
        };
        let implementor = match merge_candidates(
            candidate(&mut candidates, &implementor_key)?,
            alias_candidate(&mut alias_candidates, &implementor_key)?,
        ) {
            DbCandidate::Unique(node) => Some(node),
            DbCandidate::Missing | DbCandidate::Ambiguous => None,
        };
        let trait_ = match merge_candidates(
            candidate(&mut candidates, &trait_key)?,
            alias_candidate(&mut alias_candidates, &trait_key)?,
        ) {
            DbCandidate::Unique(node) => Some(node),
            DbCandidate::Missing | DbCandidate::Ambiguous => None,
        };
        if (old_implementor, old_trait) != (implementor, trait_) {
            update
                .execute(params![implementor, trait_, implementation_id])
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

#[derive(Clone, Copy)]
enum DbCandidate {
    Missing,
    Unique(i64),
    Ambiguous,
}

fn merge_candidates(left: DbCandidate, right: DbCandidate) -> DbCandidate {
    match (left, right) {
        (DbCandidate::Ambiguous, _) | (_, DbCandidate::Ambiguous) => DbCandidate::Ambiguous,
        (DbCandidate::Unique(left), DbCandidate::Unique(right)) if left != right => {
            DbCandidate::Ambiguous
        }
        (DbCandidate::Unique(target), _) | (_, DbCandidate::Unique(target)) => {
            DbCandidate::Unique(target)
        }
        (DbCandidate::Missing, DbCandidate::Missing) => DbCandidate::Missing,
    }
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

fn alias_candidate(statement: &mut rusqlite::Statement<'_>, key: &str) -> Result<DbCandidate> {
    let (total, resolved, distinct, target) = statement
        .query_row([key], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })
        .map_err(db_error)?;
    Ok(if total == 0 {
        DbCandidate::Missing
    } else if resolved != total {
        DbCandidate::Ambiguous
    } else if distinct == 0 {
        DbCandidate::Missing
    } else if distinct != 1 {
        DbCandidate::Ambiguous
    } else {
        DbCandidate::Unique(target.ok_or_else(|| "database alias target is invalid".to_owned())?)
    })
}

fn insert_graph(
    tx: &Transaction<'_>,
    graph: &Graph,
    cancelled: &AtomicBool,
    delta: bool,
) -> Result<(Vec<i64>, Vec<i64>)> {
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
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(db_error)?;
        for file in graph.files.iter().filter(|file| !delta || file.replace) {
            check_cancelled(cancelled)?;
            let byte_size = i64::try_from(file.byte_size)
                .map_err(|_| "file size exceeds SQLite range".to_owned())?;
            insert
                .execute(params![
                    file.path,
                    file.language.as_str(),
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

    let mut implementation_ids = Vec::with_capacity(graph.trait_implementations.len());
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO trait_implementations(
                    file_id, implementor_key, trait_key, line_start, line_end
                 ) VALUES(?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(db_error)?;
        for implementation in &graph.trait_implementations {
            check_cancelled(cancelled)?;
            let (file_id, _) = files
                .get(implementation.file_key.as_str())
                .ok_or_else(|| "trait implementation references an unknown file".to_owned())?;
            insert
                .execute(params![
                    file_id,
                    implementation.implementor_key,
                    implementation.trait_key,
                    implementation.line_start,
                    implementation.line_end
                ])
                .map_err(db_error)?;
            implementation_ids.push(tx.last_insert_rowid());
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
                "INSERT INTO refs(source_id, kind, line, alias_key, resolved_target_id)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
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
                    reference.alias_key,
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
    Ok((reference_ids, implementation_ids))
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
            alias_key TEXT CHECK(alias_key IS NULL OR length(alias_key)>0),
            resolved_target_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL
         );
         CREATE INDEX refs_source_target
             ON refs(source_id, kind, resolved_target_id);
         CREATE INDEX refs_target_source
             ON refs(resolved_target_id, kind, source_id);
         CREATE INDEX refs_alias
             ON refs(alias_key, resolved_target_id) WHERE alias_key IS NOT NULL;
         CREATE TABLE ref_keys(
            ref_id INTEGER NOT NULL REFERENCES refs(id) ON DELETE CASCADE,
            rank INTEGER NOT NULL CHECK(rank>=0),
            key TEXT NOT NULL,
            PRIMARY KEY(ref_id, rank)
         ) WITHOUT ROWID;
         CREATE INDEX ref_keys_key ON ref_keys(key, ref_id, rank);
         CREATE TABLE trait_implementations(
            id INTEGER PRIMARY KEY,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            implementor_key TEXT NOT NULL CHECK(length(implementor_key)>0),
            trait_key TEXT NOT NULL CHECK(length(trait_key)>0),
            line_start INTEGER NOT NULL CHECK(line_start>0),
            line_end INTEGER NOT NULL CHECK(line_end>=line_start),
            resolved_implementor_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
            resolved_trait_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL
         );
         CREATE INDEX trait_implementations_file_lines
             ON trait_implementations(file_id, line_start, line_end, id);
         CREATE INDEX trait_implementations_implementor_key
             ON trait_implementations(implementor_key, id);
         CREATE INDEX trait_implementations_trait_key
             ON trait_implementations(trait_key, id);
         CREATE INDEX trait_implementations_resolved_implementor
             ON trait_implementations(resolved_implementor_id, resolved_trait_id, id)
             WHERE resolved_implementor_id IS NOT NULL;
         CREATE INDEX trait_implementations_resolved_trait
             ON trait_implementations(resolved_trait_id, resolved_implementor_id, id)
             WHERE resolved_trait_id IS NOT NULL;
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
         PRAGMA user_version=4;",
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

fn load_nodes(connection: &Connection, ids: &[i64]) -> Result<Vec<RowNode>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (1..=ids.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT n.id, n.kind, n.name, f.path, n.line_start
           FROM nodes n JOIN files f ON f.id=n.file_id
          WHERE n.id IN ({placeholders})"
    );
    let mut statement = connection.prepare(&sql).map_err(db_error)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(ids), |row| {
            Ok(RowNode {
                id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(2)?,
                path: row.get(3)?,
                line: row.get(4)?,
            })
        })
        .map_err(db_error)?;
    let mut nodes = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?
        .into_iter()
        .map(|node| (node.id, node))
        .collect::<HashMap<_, _>>();
    ids.iter()
        .map(|id| {
            nodes
                .remove(id)
                .ok_or_else(|| "changed node not found".to_owned())
        })
        .collect()
}

fn load_neighbors(
    connection: &Connection,
    id: i64,
    limit: usize,
    include_members: bool,
    include_traits: bool,
) -> Result<(Vec<(String, RowNode)>, bool)> {
    let queries = [
        (
            "member ->",
            true,
            false,
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
               FROM nodes n JOIN files f ON f.id=n.file_id
              WHERE n.parent_id=?1
              ORDER BY n.kind, n.line_start, n.id LIMIT ?2",
        ),
        (
            "test <-",
            false,
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
            false,
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
               FROM edges e JOIN nodes n ON n.id=e.source_id
               JOIN files f ON f.id=n.file_id
              WHERE e.target_id=?1 AND e.kind='CALLS'
              ORDER BY e.source_id LIMIT ?2",
        ),
        (
            "impl <-",
            false,
            true,
            "SELECT DISTINCT n.id, n.kind, n.name, f.path, n.line_start
               FROM trait_implementations i JOIN nodes n ON n.id=i.resolved_implementor_id
               JOIN files f ON f.id=n.file_id
              WHERE i.resolved_trait_id=?1
              ORDER BY n.id LIMIT ?2",
        ),
        (
            "call ->",
            false,
            false,
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
               FROM edges e JOIN nodes n ON n.id=e.target_id
               JOIN files f ON f.id=n.file_id
              WHERE e.source_id=?1 AND e.kind IN ('CALLS','TEST_CALLS')
              ORDER BY e.kind, e.target_id LIMIT ?2",
        ),
        (
            "implements ->",
            false,
            true,
            "SELECT DISTINCT n.id, n.kind, n.name, f.path, n.line_start
               FROM trait_implementations i JOIN nodes n ON n.id=i.resolved_trait_id
               JOIN files f ON f.id=n.file_id
              WHERE i.resolved_implementor_id=?1
              ORDER BY n.id LIMIT ?2",
        ),
        (
            "in <-",
            false,
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
            false,
            "SELECT n.id, n.kind, n.name, f.path, n.line_start
               FROM edges e JOIN nodes n ON n.id=e.target_id
               JOIN files f ON f.id=n.file_id
              WHERE e.source_id=?1 AND e.kind='IMPORTS'
              ORDER BY e.target_id LIMIT ?2",
        ),
    ];
    let mut neighbors = Vec::with_capacity(limit);

    for (relation, members_only, types_only, sql) in queries {
        if (members_only && !include_members) || (types_only && !include_traits) {
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

fn parse_ref(value: &str) -> Result<(&str, &str, i64, i64)> {
    let mut parts = value.split(':');
    let version = parts.next();
    let snapshot_id = parts.next().unwrap_or_default();
    let epoch = parts.next().unwrap_or_default();
    let raw_generation = parts.next().unwrap_or_default();
    let generation = raw_generation
        .parse::<i64>()
        .ok()
        .filter(|value| raw_generation == value.to_string())
        .ok_or_else(|| "invalid node_ref".to_owned())?;
    let raw_id = parts.next().unwrap_or_default();
    let id = raw_id
        .parse::<i64>()
        .ok()
        .filter(|value| raw_id == value.to_string())
        .ok_or_else(|| "invalid node_ref".to_owned())?;
    if version != Some("n1")
        || snapshot_id.len() != 64
        || !snapshot_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || epoch.len() != 8
        || !epoch.bytes().all(|byte| byte.is_ascii_hexdigit())
        || generation < 0
        || id <= 0
        || parts.next().is_some()
    {
        Err("invalid node_ref".into())
    } else {
        Ok((snapshot_id, epoch, generation, id))
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

    const SNAPSHOT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn sealed_image_is_single_file_and_read_only() {
        let root = std::env::temp_dir().join(format!(
            "graphr-sealed-image-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("graph.db");
        let cancelled = AtomicBool::new(false);
        let mut store = Store::open_private_image(&path, &cancelled).unwrap();
        let (indexed, _, ()) = store
            .index_with(&cancelled, |_full, _existing| {
                Ok((single_node_graph("sealed"), ()))
            })
            .unwrap();

        let sealed = store.seal(&cancelled).unwrap();

        assert_eq!(sealed, indexed);
        assert_eq!(validate_image(&path).unwrap(), indexed);
        assert_eq!(
            fs::read_dir(&root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            [std::ffi::OsString::from("graph.db")]
        );
        let reader = Store::open_reader(&path).unwrap();
        assert!(reader.connection.execute("DELETE FROM state", []).is_err());
        drop(reader);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validate_image_rejects_a_wal_mode_database() {
        let root = std::env::temp_dir().join(format!(
            "graphr-wal-image-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("graph.db");
        let cancelled = AtomicBool::new(false);
        let mut store = Store::open_private_image(&path, &cancelled).unwrap();
        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((single_node_graph("wal"), ()))
            })
            .unwrap();
        drop(store);

        assert_eq!(
            fs::read_dir(&root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            [std::ffi::OsString::from("graph.db")]
        );
        assert!(validate_image(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validate_image_rejects_any_sqlite_sidecar() {
        let root = std::env::temp_dir().join(format!(
            "graphr-sidecar-image-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("graph.db");
        let cancelled = AtomicBool::new(false);
        let mut store = Store::open_private_image(&path, &cancelled).unwrap();
        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((single_node_graph("sidecar"), ()))
            })
            .unwrap();
        store.seal(&cancelled).unwrap();

        let mut rejected = Vec::new();
        for name in ["graph.db-wal", "graph.db-shm", "graph.db-journal"] {
            let sidecar = root.join(name);
            fs::write(&sidecar, b"").unwrap();
            rejected.push(validate_image(&path).is_err());
            fs::remove_file(sidecar).unwrap();
        }

        assert_eq!(rejected, [true, true, true]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn literal_queries_do_not_expose_fts_syntax() {
        assert_eq!(
            literal_fts("dispatch OR *").unwrap(),
            "\"dispatch\"* AND \"OR\"*"
        );
        assert!(literal_fts("***").is_err());
    }

    #[test]
    fn search_query_uses_the_bounded_fts_plan() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((single_node_graph("common"), ()))
            })
            .unwrap();

        let mut statement = store
            .connection
            .prepare(&format!("EXPLAIN QUERY PLAN {SEARCH_SQL}"))
            .unwrap();
        let plan = statement
            .query_map(params!["\"common\"*", Option::<&str>::None, 21], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(
            plan.iter()
                .any(|step| step.contains("SCAN nodes_fts VIRTUAL TABLE INDEX"))
        );
        assert!(plan.iter().all(|step| {
            !step.contains("TEMP B-TREE") && step != "SCAN n" && !step.starts_with("SCAN n ")
        }));
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
            node.line(SNAPSHOT, &state, None, 180)
                .unwrap()
                .unwrap()
                .contains("\\u{1b}")
        );
        node.name = "é".repeat(100);
        assert!(node.line(SNAPSHOT, &state, None, 20).unwrap().is_none());
        node.kind = "bogus".into();
        assert!(node.line(SNAPSHOT, &state, None, 180).is_err());
    }

    #[test]
    fn node_references_bind_snapshot_before_graph_state() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((single_node_graph("bound"), ()))
            })
            .unwrap();
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let output = store.search(&first, "bound", None, 1).unwrap();
        let node_ref = output.split_whitespace().next().unwrap();

        assert_eq!(
            store.view(&second, node_ref, 1, 10).unwrap_err(),
            "node_snapshot_mismatch"
        );
        assert_eq!(
            store
                .view(&first, &node_ref.replacen(":1:", ":01:", 1), 1, 10)
                .unwrap_err(),
            "invalid node_ref"
        );
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
            "graphr-store-{}-{}.db",
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

        assert!(Store::open_private_image(&path, &AtomicBool::new(false)).is_err());
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
        };
        let graph = Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                language: Language::Rust,
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
            .view(
                SNAPSHOT,
                &format!("n1:{SNAPSHOT}:{}:{}:1", state.epoch, state.generation),
                3,
                2,
            )
            .unwrap();
        assert!(output.contains("call ->"));
        assert!(!output.contains(TRUNCATED.trim()));
    }

    #[test]
    fn traversal_reaches_six_hops_but_not_seven() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let mut graph = single_node_graph("n0");
        for index in 1_u32..=7 {
            let key = format!("n{index}");
            graph.nodes.push(function_node(&key, index + 1));
            graph.edges.push(EdgeInput {
                source_key: format!("n{}", index - 1),
                target_key: key,
                kind: EdgeKind::Calls,
                support_count: 1,
            });
        }
        let cancelled = AtomicBool::new(false);
        let (state, _, ()) = store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let output = store
            .view(
                SNAPSHOT,
                &format!("n1:{SNAPSHOT}:{}:{}:1", state.epoch, state.generation),
                6,
                50,
            )
            .unwrap();
        assert!(output.contains(" n6 "), "{output}");
        assert!(!output.contains(" n7 "), "{output}");
        assert!(output.contains(TRUNCATED.trim()), "{output}");

        let changes = WorktreeChanges {
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                whole_file: false,
                spans: vec![LineSpan { start: 2, end: 2 }],
                report_unmapped: false,
            }],
            records: vec![],
            paths: vec![],
            source_patch: String::new(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        let output = store
            .changes(
                SNAPSHOT,
                &changes,
                6,
                50,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(output.contains(" n6 "), "{output}");
        assert!(!output.contains(" n7 "), "{output}");
        assert!(output.contains("neighborhood_omitted=false"), "{output}");
        assert!(output.contains("coverage status=complete"), "{output}");
        assert!(!output.contains(TRUNCATED.trim()), "{output}");

        assert!(
            store
                .view(
                    SNAPSHOT,
                    &format!("n1:{SNAPSHOT}:{}:{}:1", state.epoch, state.generation),
                    7,
                    50
                )
                .is_err()
        );
        assert!(
            store
                .changes(
                    SNAPSHOT,
                    &changes,
                    7,
                    50,
                    DependencyMode::Boundary,
                    &cancelled
                )
                .is_err()
        );
    }

    #[test]
    fn changes_map_gaps_and_traverse_in_global_priority_order() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
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
                language: Language::Rust,
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 64,
                replace: true,
            }],
            nodes: std::iter::once(NodeInput {
                key: "file".into(),
                file_key: "src/lib.rs".into(),
                kind: NodeKind::File,
                name: "src/lib.rs".into(),
                qualified_name: "file".into(),
                parent_key: None,
                owner_key: None,
                line_start: 1,
                line_end: 64,
                signature: String::new(),
                keys: vec![],
            })
            .chain(names.iter().enumerate().map(|(index, name)| NodeInput {
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
            }))
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
            paths: vec![],
            source_patch: String::new(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        let output = store
            .changes(
                SNAPSHOT,
                &changes,
                1,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(
            output.contains(
                "risk overall=0.4200 changed_symbols_total=1 changed_symbols_analyzed=1 changed_symbols_emitted=1 changed_symbols_omitted=0 flows_discovered=1 flows_total=unknown static_test_path_gaps=0 analysis_complete=false analysis_roots_omitted=0 deleted_paths_unanalyzed=1 neighborhood_omitted=false"
            ),
            "{output}"
        );
        assert!(output.contains("risk 0.4200"), "{output}");
        assert!(
            output.contains(
                "flow 0.1200 depth=2 nodes=3 files=1 changed=1 caller@src/lib.rs:9 -> root@src/lib.rs:2"
            ),
            "{output}"
        );
        let positions = [" root ", "test <-", "caller <-", "call ->", "import ->"]
            .map(|part| output.find(part).unwrap());
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{output}"
        );
        assert!(!output.contains(TRUNCATED.trim()), "{output}");

        let depth_zero = store
            .changes(
                SNAPSHOT,
                &changes,
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(
            depth_zero.contains("neighborhood_omitted=false"),
            "{depth_zero}"
        );
        assert!(!depth_zero.contains(TRUNCATED.trim()), "{depth_zero}");

        let unmapped = WorktreeChanges {
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                whole_file: false,
                spans: vec![],
                report_unmapped: true,
            }],
            records: vec![],
            paths: vec![],
            source_patch: String::new(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        let output = store
            .changes(
                SNAPSHOT,
                &unmapped,
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(output.contains(" File src/lib.rs src/lib.rs:1"), "{output}");
        assert!(
            output.contains("flow 0.1200 depth=2 nodes=3 files=1 changed=3"),
            "{output}"
        );
        assert_eq!(
            output
                .lines()
                .filter(|line| line.starts_with("flow "))
                .count(),
            1,
            "{output}"
        );
        assert!(output.contains("file-mapped src/lib.rs:1"), "{output}");
        assert!(output.contains("unmapped_ranges=0"), "{output}");

        let type_change = WorktreeChanges {
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                whole_file: false,
                spans: vec![LineSpan { start: 22, end: 22 }],
                report_unmapped: false,
            }],
            records: vec![],
            paths: vec![],
            source_patch: String::new(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        let output = store
            .changes(
                SNAPSHOT,
                &type_change,
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(output.contains(" Type imported src/lib.rs:11"), "{output}");
        assert!(output.contains("flow 0.1200"), "{output}");

        let mut flooded = WorktreeChanges {
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                whole_file: false,
                spans: vec![LineSpan { start: 7, end: 7 }],
                report_unmapped: true,
            }],
            records: vec![],
            paths: vec![],
            source_patch: String::new(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        for index in 0..500 {
            let path = format!("src/untracked-{index:03}.rs");
            flooded.files.push(ChangedFile {
                path: path.clone(),
                whole_file: true,
                spans: vec![],
                report_unmapped: true,
            });
            flooded.records.push(PathRecord::Untracked(path));
        }
        let output = store
            .changes(
                SNAPSHOT,
                &flooded,
                1,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(output.contains(" root "), "{output}");
        assert!(output.contains("test <-"), "{output}");
        assert!(output.contains("caller <-"), "{output}");
        assert!(
            output.contains("unmapped src/untracked-499.rs:1"),
            "{output}"
        );
        assert!(
            output.contains("coverage category=mapping status=incomplete items=500 remediation=call-index-then-restart-changes"),
            "{output}"
        );
        assert!(!output.contains(TRUNCATED.trim()), "{output}");
    }

    #[test]
    fn flow_and_risk_scores_match_crg_factors() {
        assert_eq!(flow_criticality(4, 3, 2, 1, 2, 5), 4_175);
        let risk = node_risk(1_500, 3, true, 1, false, false, true);
        assert_eq!(risk.score, 5_500);
        assert_eq!(
            (
                risk.flow_component,
                risk.test_component,
                risk.security_component,
                risk.caller_component,
            ),
            (1_500, 1_500, 2_000, 500)
        );
        assert_eq!(
            node_risk(4_000, 5, true, 10, false, false, false).score,
            6_000
        );
        let changed_test = node_risk(0, 5, false, 0, true, false, false);
        assert!(
            risk_metadata(Some(&changed_test)).contains("risk_rationale=changed-test"),
            "{}",
            risk_metadata(Some(&changed_test))
        );
        assert!(security_sensitive("verify_token", "crate::verify_token"));
        assert!(!security_sensitive("render", "crate::render"));
    }

    #[test]
    fn indirect_test_mapping_stops_at_the_request_budget() {
        let mut graph = single_node_graph("changed");
        for index in 0..=FLOW_QUERY_LIMIT {
            let name = format!("caller_{index:04}");
            graph.nodes.push(function_node(&name, index as u32 + 2));
            graph.edges.push(EdgeInput {
                source_key: name,
                target_key: "changed".into(),
                kind: EdgeKind::Calls,
                support_count: 1,
            });
        }
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        let changed_id = store
            .connection
            .query_row("SELECT id FROM nodes WHERE name='changed'", [], |row| {
                row.get(0)
            })
            .unwrap();

        let (counts, omitted) = node_risk_counts(&store.connection, &[changed_id]).unwrap();

        assert!(omitted);
        assert_eq!(counts[&changed_id].0, (FLOW_QUERY_LIMIT + 1) as u32);
        assert!(coverage_diagnostics(0, false, false, omitted, 0, 0, 1).contains(
            "coverage category=test-mapping status=incomplete items=unknown remediation=narrow-review-base-and-restart-changes"
        ));
    }

    #[test]
    fn partially_covered_hunks_report_only_residual_lines() {
        let changed = LineSpan { start: 2, end: 18 };
        let coverage = [
            LineSpan { start: 4, end: 8 },
            LineSpan { start: 12, end: 16 },
        ];
        let residual = unmapped_spans(&[changed], &coverage);
        assert_eq!(
            residual,
            [
                LineSpan { start: 2, end: 2 },
                LineSpan { start: 10, end: 10 },
                LineSpan { start: 18, end: 18 },
            ]
        );
        assert_eq!(
            unmapped_line(
                &ChangedFile {
                    path: "src/lib.rs".into(),
                    whole_file: false,
                    spans: vec![changed],
                    report_unmapped: true,
                },
                &residual,
            ),
            Some("unmapped src/lib.rs:1,5,9\n".into())
        );
    }

    #[test]
    fn adjacent_symbol_coverage_has_no_phantom_gap_and_deletions_keep_anchors() {
        assert!(
            unmapped_spans(
                &[LineSpan { start: 2, end: 4 }],
                &[LineSpan { start: 2, end: 2 }, LineSpan { start: 4, end: 4 },],
            )
            .is_empty()
        );
        assert!(
            unmapped_spans(
                &[LineSpan { start: 7, end: 7 }],
                &[LineSpan { start: 4, end: 12 }],
            )
            .is_empty()
        );
        assert_eq!(
            unmapped_spans(
                &[LineSpan { start: 13, end: 13 }],
                &[LineSpan { start: 4, end: 12 }],
            ),
            [LineSpan { start: 13, end: 13 }]
        );
    }

    #[test]
    fn whole_file_changes_map_exact_non_symbol_ranges_to_the_file() {
        let graph = |with_function: bool| Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 7,
                replace: true,
            }],
            nodes: std::iter::once(NodeInput {
                key: "file".into(),
                file_key: "src/lib.rs".into(),
                kind: NodeKind::File,
                name: "src/lib.rs".into(),
                qualified_name: "file".into(),
                parent_key: None,
                owner_key: None,
                line_start: 1,
                line_end: 7,
                signature: String::new(),
                keys: vec![],
            })
            .chain(with_function.then(|| function_node("only_symbol", 5)))
            .collect(),
            ..Graph::default()
        };
        let changes = || WorktreeChanges {
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                whole_file: true,
                spans: vec![],
                report_unmapped: true,
            }],
            records: vec![PathRecord::Untracked("src/lib.rs".into())],
            paths: vec![],
            source_patch: String::new(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph(true), ())))
            .unwrap();

        let output = store
            .changes(
                SNAPSHOT,
                &changes(),
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(
            output.contains("file-mapped src/lib.rs:1-4,6-7"),
            "{output}"
        );
        assert!(output.contains("unmapped_ranges=0"), "{output}");
        assert!(output.contains("file_mapped_ranges=2"), "{output}");

        let renamed = WorktreeChanges {
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                whole_file: true,
                spans: vec![LineSpan { start: 2, end: 2 }],
                report_unmapped: true,
            }],
            records: vec![PathRecord::Renamed(
                "src/old.rs".into(),
                "src/lib.rs".into(),
            )],
            paths: vec![],
            source_patch: String::new(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        let output = store
            .changes(
                SNAPSHOT,
                &renamed,
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(output.contains("file-mapped src/lib.rs:1\n"), "{output}");
        assert!(
            !output.contains("file-mapped src/lib.rs:1-4,6-7"),
            "{output}"
        );

        store
            .index_with(&cancelled, |_full, _existing| Ok((graph(false), ())))
            .unwrap();
        let output = store
            .changes(
                SNAPSHOT,
                &changes(),
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(output.contains("file-mapped src/lib.rs:1-7"), "{output}");
        assert!(!output.contains("file-mapped src/lib.rs:1\n"), "{output}");
    }

    #[test]
    fn deleted_only_changes_report_incomplete_analysis() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let output = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![],
                    records: vec![PathRecord::Deleted("src/removed.rs".into())],
                    paths: vec![],
                    source_patch: String::new(),
                    artifacts: Default::default(),
                    skipped_paths: 0,
                },
                0,
                10,
                DependencyMode::Boundary,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(
            output.contains("flows_discovered=0 flows_total=unknown"),
            "{output}"
        );
        assert!(output.contains("analysis_complete=false"), "{output}");
        assert!(output.contains("deleted_paths_unanalyzed=1"), "{output}");
        assert!(
            output.contains("coverage category=deleted-paths status=incomplete items=1 remediation=review-corresponding-diff-pages"),
            "{output}"
        );
    }

    #[test]
    fn mixed_hunk_maps_functions_and_syntax_glue() {
        let mut first = function_node("first", 2);
        first.line_end = 4;
        let mut second = function_node("second", 6);
        second.line_end = 8;
        let graph = Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 9,
                replace: true,
            }],
            nodes: vec![
                NodeInput {
                    key: "file".into(),
                    file_key: "src/lib.rs".into(),
                    kind: NodeKind::File,
                    name: "src/lib.rs".into(),
                    qualified_name: "file".into(),
                    parent_key: None,
                    owner_key: None,
                    line_start: 1,
                    line_end: 9,
                    signature: String::new(),
                    keys: vec![],
                },
                first,
                second,
            ],
            ..Graph::default()
        };
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let output = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![ChangedFile {
                        path: "src/lib.rs".into(),
                        whole_file: false,
                        spans: vec![LineSpan { start: 2, end: 18 }],
                        report_unmapped: true,
                    }],
                    records: vec![],
                    paths: vec![],
                    source_patch: String::new(),
                    artifacts: Default::default(),
                    skipped_paths: 0,
                },
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();

        assert!(output.contains(" Function first src/lib.rs:2"), "{output}");
        assert!(output.contains(" Function second src/lib.rs:6"), "{output}");
        assert!(output.contains("file-mapped src/lib.rs:1,5,9"), "{output}");
        assert!(!output.contains("file-mapped src/lib.rs:1-9"), "{output}");
        assert!(output.contains("coverage status=complete"), "{output}");
    }

    #[test]
    fn changes_emit_every_ranked_root_within_the_analysis_limit() {
        let graph = Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 51,
                replace: true,
            }],
            nodes: (0_u32..50)
                .map(|index| function_node(&format!("node_{index}"), index + 1))
                .chain(std::iter::once(function_node("verify_token", 51)))
                .collect(),
            ..Graph::default()
        };
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let output = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![ChangedFile {
                        path: "src/lib.rs".into(),
                        whole_file: true,
                        spans: vec![],
                        report_unmapped: false,
                    }],
                    records: vec![],
                    paths: vec![],
                    source_patch: String::new(),
                    artifacts: Default::default(),
                    skipped_paths: 0,
                },
                0,
                50,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();

        assert!(
            output.contains(
                "changed_symbols_total=51 changed_symbols_analyzed=51 changed_symbols_emitted=51 changed_symbols_omitted=0 flows_total=0 static_test_path_gaps=51 analysis_complete=true analysis_roots_omitted=0 deleted_paths_unanalyzed=0"
            ),
            "{output}"
        );
        assert!(output.contains(" no-static-test-path"), "{output}");
        assert!(
            output.contains("test_path_confidence=heuristic"),
            "{output}"
        );
        assert!(
            output.contains("test_path_provenance=resolved-static-call-graph"),
            "{output}"
        );
        assert!(
            output.contains("risk_components=flow:0.0000,test_paths:0.3000"),
            "{output}"
        );
        assert!(
            output.contains("risk_rationale=no-static-test-path"),
            "{output}"
        );
        assert!(!output.contains("test-gap"), "{output}");
        assert!(!output.contains("test_gaps="), "{output}");
        assert!(!output.contains("tests:"), "{output}");
        assert!(!output.contains("no-test-coverage"), "{output}");
        assert!(output.contains(" Function verify_token "), "{output}");
        assert!(output.contains(" Function node_49 "), "{output}");
        assert!(output.contains("neighborhood_omitted=false"), "{output}");
        assert!(!output.contains(" gaps="), "{output}");
        assert!(!output.contains(TRUNCATED.trim()), "{output}");
    }

    #[test]
    fn changed_roots_do_not_spend_the_neighborhood_limit() {
        let mut graph = single_node_graph("changed_00");
        for index in 1_u32..43 {
            graph
                .nodes
                .push(function_node(&format!("changed_{index:02}"), index + 1));
        }
        for index in 0_u32..8 {
            let name = format!("neighbor_{index}");
            graph.nodes.push(function_node(&name, 100 + index));
            graph.edges.push(EdgeInput {
                source_key: "changed_00".into(),
                target_key: name,
                kind: EdgeKind::Calls,
                support_count: 1,
            });
        }
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let output = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![ChangedFile {
                        path: "src/lib.rs".into(),
                        whole_file: false,
                        spans: vec![LineSpan { start: 2, end: 86 }],
                        report_unmapped: false,
                    }],
                    records: vec![],
                    paths: vec![],
                    source_patch: String::new(),
                    artifacts: Default::default(),
                    skipped_paths: 0,
                },
                1,
                50,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();

        assert!(output.contains("changed_symbols_total=43"), "{output}");
        assert!(output.contains("neighbor_7 src/lib.rs:107"), "{output}");
        assert!(output.contains("neighborhood_omitted=false"), "{output}");
        assert!(output.contains("coverage status=complete"), "{output}");
    }

    #[test]
    fn changes_bound_root_analysis_and_report_the_omission() {
        let graph = Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 501,
                replace: true,
            }],
            nodes: (0_u32..=500)
                .map(|index| {
                    let mut node = function_node(&format!("type_{index}"), index + 1);
                    node.kind = NodeKind::Type;
                    node
                })
                .collect(),
            ..Graph::default()
        };
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let output = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![ChangedFile {
                        path: "src/lib.rs".into(),
                        whole_file: true,
                        spans: vec![],
                        report_unmapped: false,
                    }],
                    records: vec![],
                    paths: vec![],
                    source_patch: String::new(),
                    artifacts: Default::default(),
                    skipped_paths: 0,
                },
                0,
                50,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();

        assert!(
            output.contains(
                "changed_symbols_total=501 changed_symbols_analyzed=500 changed_symbols_emitted=500 changed_symbols_omitted=1 flows_discovered=0 flows_total=unknown"
            ),
            "{output}"
        );
        assert!(output.contains("analysis_complete=false"), "{output}");
        assert!(output.contains("analysis_roots_omitted=1"), "{output}");
        assert!(output.contains("neighborhood_omitted=true"), "{output}");
        assert!(
            output.contains("coverage category=analysis-roots status=incomplete items=1 remediation=narrow-review-base-and-restart-changes"),
            "{output}"
        );
    }

    #[test]
    fn trait_implementations_resolve_incrementally_and_map_headers() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        let file = |path: &str, replace| FileInput {
            path: path.into(),
            language: Language::Rust,
            git_oid: None,
            content_hash: [0; 32],
            parse_context: String::new(),
            byte_size: 1,
            replace,
        };
        let type_node = |key: &str, path: &str, name: &str, lookup: &str| NodeInput {
            key: key.into(),
            file_key: path.into(),
            kind: NodeKind::Type,
            name: name.into(),
            qualified_name: format!("{name}@{path}"),
            parent_key: None,
            owner_key: None,
            line_start: 1,
            line_end: 1,
            signature: String::new(),
            keys: vec![lookup.into()],
        };

        let graph = Graph {
            files: vec![file("src/impl.rs", true), file("src/trait.rs", true)],
            nodes: vec![type_node("flow", "src/trait.rs", "Flow", "rust:item:Flow")],
            trait_implementations: vec![TraitImplementationInput {
                file_key: "src/impl.rs".into(),
                line_start: 10,
                line_end: 11,
                implementor_key: "rust:item:Cursor".into(),
                trait_key: "rust:item:Flow".into(),
            }],
            ..Graph::default()
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT resolved_implementor_id, resolved_trait_id
                       FROM trait_implementations",
                    [],
                    |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
                )
                .unwrap(),
            (None, Some(1))
        );

        let graph = Graph {
            files: vec![
                file("src/impl.rs", false),
                file("src/trait.rs", false),
                file("src/cursor.rs", true),
            ],
            nodes: vec![type_node(
                "cursor",
                "src/cursor.rs",
                "Cursor",
                "rust:item:Cursor",
            )],
            ..Graph::default()
        };
        let (state, _, ()) = store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        let (implementor, trait_) = store
            .connection
            .query_row(
                "SELECT resolved_implementor_id, resolved_trait_id
                   FROM trait_implementations",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap();
        assert!(
            store
                .view(
                    SNAPSHOT,
                    &format!(
                        "n1:{SNAPSHOT}:{}:{}:{implementor}",
                        state.epoch, state.generation
                    ),
                    1,
                    10,
                )
                .unwrap()
                .contains("implements ->")
        );
        assert!(
            store
                .view(
                    SNAPSHOT,
                    &format!(
                        "n1:{SNAPSHOT}:{}:{}:{trait_}",
                        state.epoch, state.generation
                    ),
                    1,
                    10,
                )
                .unwrap()
                .contains("impl <-")
        );
        let output = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![ChangedFile {
                        path: "src/impl.rs".into(),
                        whole_file: false,
                        spans: vec![LineSpan { start: 20, end: 20 }],
                        report_unmapped: true,
                    }],
                    records: vec![],
                    paths: vec![],
                    source_patch: String::new(),
                    artifacts: Default::default(),
                    skipped_paths: 0,
                },
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(
            output.contains(" Cursor ") && output.contains(" Flow "),
            "{output}"
        );

        let graph = Graph {
            files: vec![file("src/impl.rs", false), file("src/trait.rs", false)],
            ..Graph::default()
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        assert!(
            store
                .connection
                .query_row(
                    "SELECT resolved_implementor_id IS NULL FROM trait_implementations",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );

        let graph = Graph {
            files: vec![file("src/trait.rs", false)],
            ..Graph::default()
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM trait_implementations", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn failed_replacement_preserves_the_committed_graph() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
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
    fn changes_do_not_truncate_on_visited_rows_with_budget_left() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let mut graph = single_node_graph("a");
        graph.nodes.push(function_node("b", 2));
        graph.edges.extend([
            EdgeInput {
                source_key: "a".into(),
                target_key: "a".into(),
                kind: EdgeKind::Imports,
                support_count: 1,
            },
            EdgeInput {
                source_key: "a".into(),
                target_key: "b".into(),
                kind: EdgeKind::Imports,
                support_count: 1,
            },
        ]);
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let output = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![ChangedFile {
                        path: "src/lib.rs".into(),
                        whole_file: true,
                        spans: vec![],
                        report_unmapped: false,
                    }],
                    records: vec![],
                    paths: vec![],
                    source_patch: String::new(),
                    artifacts: Default::default(),
                    skipped_paths: 0,
                },
                1,
                3,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();

        assert!(!output.contains(TRUNCATED.trim()), "{output}");
    }

    #[test]
    fn boundary_neighbors_do_not_spend_the_first_party_budget() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let mut graph = single_node_graph("root");
        graph.files.extend([
            FileInput {
                path: ".cargo/vendor/sha2/src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [1; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
            },
            FileInput {
                path: "src/other.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [2; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
            },
        ]);
        for index in 0..100 {
            let key = format!("vendor_{index:03}");
            let mut node = function_node(&key, 1);
            node.file_key = ".cargo/vendor/sha2/src/lib.rs".into();
            graph.nodes.push(node);
            graph.edges.push(EdgeInput {
                source_key: "root".into(),
                target_key: key,
                kind: EdgeKind::Calls,
                support_count: 1,
            });
        }
        let mut first_party = function_node("first_party", 1);
        first_party.file_key = "src/other.rs".into();
        graph.nodes.push(first_party);
        graph.edges.push(EdgeInput {
            source_key: "root".into(),
            target_key: "first_party".into(),
            kind: EdgeKind::Calls,
            support_count: 1,
        });
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let output = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![ChangedFile {
                        path: "src/lib.rs".into(),
                        whole_file: true,
                        spans: vec![],
                        report_unmapped: false,
                    }],
                    records: vec![],
                    paths: vec![],
                    source_patch: String::new(),
                    artifacts: Default::default(),
                    skipped_paths: 0,
                },
                1,
                3,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();

        assert!(output.contains("first_party src/other.rs:1"), "{output}");
        assert!(
            output.contains("call -> dependency-boundary package=sha2"),
            "{output}"
        );
        assert!(output.contains("neighborhood_omitted=false"), "{output}");
        assert!(!output.contains("vendor_099"), "{output}");
    }

    #[test]
    fn boundary_flows_collapse_vendor_fanout_before_the_scan_budget() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let mut graph = single_node_graph("root");
        graph.files.extend([
            FileInput {
                path: ".cargo/vendor/sha2/src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [1; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
            },
            FileInput {
                path: "src/other.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [2; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
            },
            FileInput {
                path: ".cargo/vendor/block-buffer/src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [3; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
            },
        ]);
        for index in 0..FLOW_SCAN_LIMIT {
            let key = format!("vendor_{index:03}");
            let mut node = function_node(&key, 1);
            node.file_key = ".cargo/vendor/sha2/src/lib.rs".into();
            graph.nodes.push(node);
            graph.edges.push(EdgeInput {
                source_key: "root".into(),
                target_key: key,
                kind: EdgeKind::Calls,
                support_count: 1,
            });
        }
        let mut nested_dependency = function_node("nested_dependency", 1);
        nested_dependency.file_key = ".cargo/vendor/block-buffer/src/lib.rs".into();
        graph.nodes.push(nested_dependency);
        graph.edges.push(EdgeInput {
            source_key: "vendor_000".into(),
            target_key: "nested_dependency".into(),
            kind: EdgeKind::Calls,
            support_count: 1,
        });
        let mut first_party = function_node("first_party", 1);
        first_party.file_key = "src/other.rs".into();
        graph.nodes.push(first_party);
        graph.edges.push(EdgeInput {
            source_key: "root".into(),
            target_key: "first_party".into(),
            kind: EdgeKind::Calls,
            support_count: 1,
        });
        let cancelled = AtomicBool::new(false);
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        let entry = load_flow_nodes(&store.connection, &[1]).unwrap().remove(0);
        let mut query_budget = FLOW_QUERY_LIMIT;
        let (flow, omitted) = trace_flow(
            &store.connection,
            entry,
            &HashSet::from([1]),
            &mut query_budget,
            DependencyMode::Boundary,
            &cancelled,
        )
        .unwrap();
        let flow = flow.unwrap();

        assert!(!omitted);
        assert!(flow.nodes.iter().any(|node| node.name == "first_party"));
        assert!(
            !flow
                .nodes
                .iter()
                .any(|node| node.name == "nested_dependency")
        );
        assert_eq!(
            flow.nodes
                .iter()
                .filter(|node| dependency_package(&node.path).is_some())
                .count(),
            1
        );
    }

    #[test]
    fn neighbor_queries_stop_at_the_shared_budget() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let mut graph = single_node_graph("root");
        for index in 0..100 {
            let key = format!("child-{index}");
            graph.nodes.push(function_node(&key, 1));
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

        let (neighbors, more) = load_neighbors(&store.connection, 1, 3, false, false).unwrap();
        assert_eq!(neighbors.len(), 3);
        assert!(more);
    }

    #[test]
    fn boundary_flow_keeps_one_dependency_terminal() {
        let entry = FlowNode {
            id: 1,
            kind: "function".into(),
            name: "entry".into(),
            qualified_name: "entry".into(),
            path: "src/lib.rs".into(),
            line: 1,
        };
        let changed = FlowNode {
            id: 2,
            kind: "function".into(),
            name: "digest".into(),
            qualified_name: "digest".into(),
            path: "src/canonical.rs".into(),
            line: 2,
        };
        let helper = FlowNode {
            id: 3,
            kind: "function".into(),
            name: "helper".into(),
            qualified_name: "helper".into(),
            path: "src/canonical.rs".into(),
            line: 3,
        };
        let dependency = FlowNode {
            id: 4,
            kind: "function".into(),
            name: "internal_digest".into(),
            qualified_name: "internal_digest".into(),
            path: ".cargo/vendor/sha2/src/lib.rs".into(),
            line: 4,
        };
        let dependency_internal = FlowNode {
            id: 5,
            kind: "function".into(),
            name: "compress".into(),
            qualified_name: "compress".into(),
            path: ".cargo/vendor/sha2/src/internal.rs".into(),
            line: 5,
        };
        let flow = AffectedFlow {
            entry: entry.clone(),
            nodes: vec![entry, changed, helper, dependency, dependency_internal],
            parents: HashMap::from([(2, 1), (3, 2), (4, 3), (5, 4)]),
            changed: vec![2],
            depth: 4,
            file_count: 4,
            criticality: 1_000,
        };

        assert_eq!(
            flow_line(&flow, DependencyMode::Boundary).unwrap(),
            "flow 0.1000 depth=4 nodes=5 files=4 changed=1 entry@src/lib.rs:1 -> digest@src/canonical.rs:2 -> helper@src/canonical.rs:3 -> dependency-boundary[sha2]\n"
        );
    }

    fn single_node_graph(name: &str) -> Graph {
        Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
            }],
            nodes: vec![function_node(name, 1)],
            ..Graph::default()
        }
    }

    fn function_node(name: &str, line: u32) -> NodeInput {
        NodeInput {
            key: name.into(),
            file_key: "src/lib.rs".into(),
            kind: NodeKind::Function,
            name: name.into(),
            qualified_name: name.into(),
            parent_key: None,
            owner_key: None,
            line_start: line,
            line_end: line,
            signature: String::new(),
            keys: vec![],
        }
    }
}
