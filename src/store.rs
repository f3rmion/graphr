use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::evidence::CoverageFormat;
use crate::git::{
    ChangedFile, DependencyMode, Language, LineSpan, PathRecord, WorktreeChanges,
    change_content_complete, dependency_package,
};

pub(crate) const SCHEMA_VERSION: i64 = 8;
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
const DOT_BUDGET: usize = 8 * 1024;
const DOT_LABEL_PART_LIMIT: usize = 160;
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

#[cfg(test)]
thread_local! {
    static AFTER_REFERENCE_CANDIDATE_PASS_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn set_after_reference_candidate_pass_hook(hook: impl FnOnce() + 'static) {
    AFTER_REFERENCE_CANDIDATE_PASS_HOOK.with(|slot| slot.replace(Some(Box::new(hook))));
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionState {
    Pending,
    Resolved,
    Missing,
    Ambiguous,
}

impl ResolutionState {
    pub fn db(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
            Self::Missing => "missing",
            Self::Ambiguous => "ambiguous",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "resolved" => Some(Self::Resolved),
            "missing" => Some(Self::Missing),
            "ambiguous" => Some(Self::Ambiguous),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum GapCategory {
    Source,
    Parse,
    Relation,
    Macro,
    Generated,
    Coverage,
    Language,
    Boundary,
}

impl GapCategory {
    pub fn db(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Parse => "parse",
            Self::Relation => "relation",
            Self::Macro => "macro",
            Self::Generated => "generated",
            Self::Coverage => "coverage",
            Self::Language => "language",
            Self::Boundary => "boundary",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "source" => Some(Self::Source),
            "parse" => Some(Self::Parse),
            "relation" => Some(Self::Relation),
            "macro" => Some(Self::Macro),
            "generated" => Some(Self::Generated),
            "coverage" => Some(Self::Coverage),
            "language" => Some(Self::Language),
            "boundary" => Some(Self::Boundary),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum GapReason {
    UnsafePath,
    NonRegular,
    Unmerged,
    Oversized,
    InvalidUtf8,
    MissingDuringRead,
    ParserError,
    ParserNoTree,
    DynamicOrUnsupportedDispatch,
    MacroExpansionUnavailable,
    GeneratedOutputUnobserved,
    GeneratedOutputAmbiguous,
    ExternalDependency,
    DependencyCollapsed,
    LanguageNotIndexed,
    CoverageUnmappedFile,
    CoverageUnmappedRegion,
    MissingTestContext,
    AmbiguousTestContext,
}

impl GapReason {
    pub fn db(self) -> &'static str {
        match self {
            Self::UnsafePath => "unsafe-path",
            Self::NonRegular => "non-regular",
            Self::Unmerged => "unmerged",
            Self::Oversized => "oversized",
            Self::InvalidUtf8 => "invalid-utf8",
            Self::MissingDuringRead => "missing-during-read",
            Self::ParserError => "parser-error",
            Self::ParserNoTree => "parser-no-tree",
            Self::DynamicOrUnsupportedDispatch => "dynamic-or-unsupported-dispatch",
            Self::MacroExpansionUnavailable => "macro-expansion-unavailable",
            Self::GeneratedOutputUnobserved => "generated-output-unobserved",
            Self::GeneratedOutputAmbiguous => "generated-output-ambiguous",
            Self::ExternalDependency => "external-dependency",
            Self::DependencyCollapsed => "dependency-collapsed",
            Self::LanguageNotIndexed => "language-not-indexed",
            Self::CoverageUnmappedFile => "coverage-unmapped-file",
            Self::CoverageUnmappedRegion => "coverage-unmapped-region",
            Self::MissingTestContext => "missing-test-context",
            Self::AmbiguousTestContext => "ambiguous-test-context",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "unsafe-path" => Some(Self::UnsafePath),
            "non-regular" => Some(Self::NonRegular),
            "unmerged" => Some(Self::Unmerged),
            "oversized" => Some(Self::Oversized),
            "invalid-utf8" => Some(Self::InvalidUtf8),
            "missing-during-read" => Some(Self::MissingDuringRead),
            "parser-error" => Some(Self::ParserError),
            "parser-no-tree" => Some(Self::ParserNoTree),
            "dynamic-or-unsupported-dispatch" => Some(Self::DynamicOrUnsupportedDispatch),
            "macro-expansion-unavailable" => Some(Self::MacroExpansionUnavailable),
            "generated-output-unobserved" => Some(Self::GeneratedOutputUnobserved),
            "generated-output-ambiguous" => Some(Self::GeneratedOutputAmbiguous),
            "external-dependency" => Some(Self::ExternalDependency),
            "dependency-collapsed" => Some(Self::DependencyCollapsed),
            "language-not-indexed" => Some(Self::LanguageNotIndexed),
            "coverage-unmapped-file" => Some(Self::CoverageUnmappedFile),
            "coverage-unmapped-region" => Some(Self::CoverageUnmappedRegion),
            "missing-test-context" => Some(Self::MissingTestContext),
            "ambiguous-test-context" => Some(Self::AmbiguousTestContext),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeledSiteKind {
    GeneratedInclusion,
    TestRegistration,
    StaticExport,
}

impl ModeledSiteKind {
    pub fn db(self) -> &'static str {
        match self {
            Self::GeneratedInclusion => "generated-inclusion",
            Self::TestRegistration => "test-registration",
            Self::StaticExport => "static-export",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "generated-inclusion" => Some(Self::GeneratedInclusion),
            "test-registration" => Some(Self::TestRegistration),
            "static-export" => Some(Self::StaticExport),
            _ => None,
        }
    }
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
    pub observed_relation_sites: u32,
}

pub struct StoredFile {
    id: i64,
    pub language: Language,
    pub git_oid: Option<String>,
    pub content_hash: [u8; 32],
    pub parse_context: String,
    pub byte_size: u64,
    pub observed_relation_sites: u32,
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
    pub resolution: ResolutionState,
}

pub struct ModeledSiteInput {
    pub file_key: String,
    pub source_key: Option<String>,
    pub kind: ModeledSiteKind,
    pub line_start: u32,
    pub line_end: u32,
    pub target_hint: Option<String>,
    pub parse_context: Option<String>,
}

pub struct GapInput {
    pub file_key: Option<String>,
    pub source_key: Option<String>,
    pub run_key: Option<String>,
    pub path: Option<String>,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    pub category: GapCategory,
    pub reason: GapReason,
    pub target_hint: Option<String>,
    pub occurrences: u32,
    pub relation_site: bool,
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ArtifactRole {
    Manifest,
    Input,
    GeneratedRust,
    CoverageReport,
}

impl ArtifactRole {
    pub const fn db(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Input => "input",
            Self::GeneratedRust => "generated-rust",
            Self::CoverageReport => "coverage-report",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "manifest" => Some(Self::Manifest),
            "input" => Some(Self::Input),
            "generated-rust" => Some(Self::GeneratedRust),
            "coverage-report" => Some(Self::CoverageReport),
            _ => None,
        }
    }
}

pub struct ImportedArtifactInput {
    pub key: String,
    pub path: String,
    pub role: ArtifactRole,
    pub content_hash: [u8; 32],
    pub byte_size: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverageBranchKind {
    TrueOutcome,
    FalseOutcome,
    Arc,
}

impl CoverageBranchKind {
    const fn db(self) -> &'static str {
        match self {
            Self::TrueOutcome => "true-outcome",
            Self::FalseOutcome => "false-outcome",
            Self::Arc => "arc",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "true-outcome" => Some(Self::TrueOutcome),
            "false-outcome" => Some(Self::FalseOutcome),
            "arc" => Some(Self::Arc),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvidenceLineSpan {
    pub start: u32,
    pub end: u32,
}

pub struct GeneratedInclusionCandidate {
    pub parse_context: String,
}

#[derive(Clone)]
pub struct ProvenanceInput {
    pub input_key: String,
    pub input_lines: EvidenceLineSpan,
    pub generator_path: String,
    pub generator_lines: EvidenceLineSpan,
    pub output_key: String,
    pub output_lines: EvidenceLineSpan,
}

pub struct CoverageRunInput {
    pub key: String,
    pub format: CoverageFormat,
    pub report_key: String,
    pub run_label: String,
    pub test_name: Option<String>,
}

pub struct CoverageRegionInput {
    pub run_key: String,
    pub path: Option<String>,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub execution_count: u64,
    pub context: Option<String>,
}

pub struct CoverageBranchInput {
    pub run_key: String,
    pub path: Option<String>,
    pub start_line: i64,
    pub start_column: u32,
    pub end_line: i64,
    pub end_column: u32,
    pub target_line: Option<i64>,
    pub kind: CoverageBranchKind,
    pub execution_count: u64,
}

#[derive(Default)]
pub struct EvidenceInput {
    pub artifacts: Vec<ImportedArtifactInput>,
    pub provenance: Vec<ProvenanceInput>,
    pub runs: Vec<CoverageRunInput>,
    pub regions: Vec<CoverageRegionInput>,
    pub branches: Vec<CoverageBranchInput>,
    pub gaps: Vec<GapInput>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvidenceStats {
    pub generated_files: usize,
    pub artifacts: usize,
    pub provenance_links: usize,
    pub runs: usize,
    pub regions: usize,
    pub branches: usize,
    pub gaps: usize,
}

#[derive(Default)]
pub struct Graph {
    pub files: Vec<FileInput>,
    pub nodes: Vec<NodeInput>,
    pub refs: Vec<RefInput>,
    pub trait_implementations: Vec<TraitImplementationInput>,
    pub edges: Vec<EdgeInput>,
    pub modeled_sites: Vec<ModeledSiteInput>,
    pub gaps: Vec<GapInput>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct State {
    pub epoch: String,
    pub generation: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletenessStatus {
    Complete,
    Partial,
    NotApplicable,
}

impl CompletenessStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::NotApplicable => "not-applicable",
        }
    }
}

pub struct ChangeReview {
    pub graph: String,
    pub dot: String,
    pub evidence: String,
    pub static_status: CompletenessStatus,
    pub dynamic_status: CompletenessStatus,
}

pub struct Store {
    connection: Connection,
}

impl Store {
    pub(crate) fn open_private_image(path: &Path, cancelled: &AtomicBool) -> Result<Self> {
        Self::open_with_parent(path, cancelled)
    }

    fn open_with_parent(path: &Path, cancelled: &AtomicBool) -> Result<Self> {
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

        // `SQLITE_OPEN_NOFOLLOW` rejects a symlinked *ancestor*, not just a
        // symlinked final component, and every database here is reached
        // through the cache. Sending it unconditionally is therefore safe only
        // because `workspace::cache_paths` refuses a `common_git_dir` that is
        // not already its own canonicalisation. Relax that clause and every
        // open below starts failing on any symlinked path.
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
        require_graph_invariants(&self.connection, cancelled)?;
        require_evidence_invariants(&self.connection, cancelled)?;
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
            refresh_script_export_methods(&tx, cancelled)?;
            resolve_trait_implementations(&tx, implementations.into_iter().collect(), cancelled)?;
            graph.files.len()
        } else {
            apply_incremental(&tx, &graph, &existing, cancelled)?
        };
        require_graph_invariants(&tx, cancelled)?;
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

    pub fn replace_evidence(
        &mut self,
        mut generated_graph: Graph,
        evidence: &EvidenceInput,
        cancelled: &AtomicBool,
    ) -> Result<EvidenceStats> {
        let tx = begin_immediate(&self.connection, cancelled)?;
        check_cancelled(cancelled)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        if version != SCHEMA_VERSION {
            return Err("database schema mismatch".into());
        }
        read_state(&tx)?;
        let existing = load_stored_files(&tx)?;
        generated_graph
            .gaps
            .extend(load_source_global_gaps(&tx, cancelled)?);
        let old_generated = tx
            .prepare(
                "SELECT path FROM imported_artifacts WHERE role='generated-rust' ORDER BY path",
            )
            .map_err(db_error)?
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<HashSet<_>>>()
            .map_err(db_error)?;
        let new_generated = generated_graph
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<HashSet<_>>();
        let generated_file_count = new_generated.len();
        if new_generated.len() != generated_graph.files.len()
            || new_generated
                .iter()
                .any(|path| existing.contains_key(path) && !old_generated.contains(path))
        {
            return Err("generated output conflicts with a source file".into());
        }
        for (path, file) in &existing {
            if old_generated.contains(path) || new_generated.contains(path) {
                continue;
            }
            generated_graph.files.push(FileInput {
                path: path.clone(),
                language: file.language,
                git_oid: file.git_oid.clone(),
                content_hash: file.content_hash,
                parse_context: file.parse_context.clone(),
                byte_size: file.byte_size,
                replace: false,
                observed_relation_sites: file.observed_relation_sites,
            });
        }
        generated_graph
            .files
            .sort_by(|left, right| left.path.cmp(&right.path));
        apply_incremental(&tx, &generated_graph, &existing, cancelled)?;

        tx.execute("DELETE FROM imported_artifacts", [])
            .map_err(db_error)?;
        let stats = insert_evidence(&tx, evidence, generated_file_count, cancelled)?;
        require_graph_invariants(&tx, cancelled)?;
        require_evidence_invariants(&tx, cancelled)?;
        tx.execute(
            "UPDATE state SET generation=generation+1 WHERE singleton=1",
            [],
        )
        .map_err(db_error)?;
        check_cancelled(cancelled)?;
        tx.commit().map_err(db_error)?;
        Ok(stats)
    }

    pub fn generated_inclusion_candidates(
        &self,
        output_basename: &str,
    ) -> Result<Vec<GeneratedInclusionCandidate>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT m.parse_context FROM modeled_sites m
                  WHERE m.kind='generated-inclusion' AND m.target_hint=?1
                    AND m.parse_context IS NOT NULL
                  ORDER BY m.file_id, m.line_start, m.id LIMIT 2",
            )
            .map_err(db_error)?;
        statement
            .query_map([output_basename], |row| {
                Ok(GeneratedInclusionCandidate {
                    parse_context: row.get(0)?,
                })
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)
    }

    pub fn evidence_only_paths(&self) -> Result<BTreeSet<String>> {
        self.connection
            .prepare(
                "SELECT path FROM imported_artifacts
                  WHERE role IN ('manifest','generated-rust','coverage-report')
                  ORDER BY path",
            )
            .map_err(db_error)?
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .map_err(db_error)
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
        let root_end = if root.kind == "file" {
            u32::MAX
        } else {
            tx.query_row("SELECT line_end FROM nodes WHERE id=?1", [root_id], |row| {
                row.get(0)
            })
            .map_err(db_error)?
        };
        let mut evidence_scope = CoverageScope::node(
            root_id,
            &root.path,
            root.line,
            root_end,
            root.kind == "file",
        );
        evidence_scope.expand_provenance(&tx)?;
        let static_gaps = static_gap_records(&tx, &evidence_scope)?;
        if !static_gaps.is_empty() {
            lines.extend(static_gaps.split_inclusive('\n').map(str::to_owned));
        }
        let evidence = render_evidence(&tx, Some(&evidence_scope))?;
        if !evidence.text.is_empty() {
            lines.extend(evidence.text.split_inclusive('\n').map(str::to_owned));
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
    ) -> Result<ChangeReview> {
        if depth > 6 || !(1..=50).contains(&max_nodes) {
            return Err("invalid changes parameters".into());
        }
        check_cancelled(cancelled)?;
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
        let evidence_scope = CoverageScope::changes(changes, dependency_mode, &self.connection)?;
        let evidence = render_evidence(&self.connection, Some(&evidence_scope))?;
        if changes.is_empty() && changes.files.is_empty() && changes.records.is_empty() {
            return Ok(ChangeReview {
                graph: "no changes\n".into(),
                dot: no_change_dot(snapshot_id, "empty worktree"),
                evidence: evidence.text,
                static_status: CompletenessStatus::Complete,
                dynamic_status: evidence.status,
            });
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
            let traversal_complete = deleted_paths_unanalyzed == 0;
            let mut output = format!(
                "risk overall=0.0000 changed_symbols_total=0 changed_symbols_analyzed=0 changed_symbols_emitted=0 changed_symbols_omitted=0 {flow_accounting} static_test_path_gaps=0 traversal_complete={traversal_complete} analysis_roots_omitted=0 deleted_paths_unanalyzed={deleted_paths_unanalyzed} neighborhood_omitted=false unmapped_ranges=0 file_mapped_ranges=0 dependency_analysis={} {}\n",
                dependency_analysis(dependency_mode),
                risk_metadata(None),
            );
            let accounting = static_accounting(&self.connection, &evidence_scope)?;
            let content_complete = change_content_complete(changes, dependency_mode);
            output.push_str(&assurance_preamble(
                &accounting,
                content_complete,
                true,
                traversal_complete,
                &evidence,
            ));
            let static_status = accounting.overall(content_complete, true, traversal_complete);
            let dynamic_status = evidence.status;
            return Ok(ChangeReview {
                graph: output,
                dot: change_dot(
                    snapshot_id,
                    &[],
                    &ChangeAnalysis::default(),
                    &ChangeCalls::default(),
                    (depth, max_nodes),
                    dependency_mode,
                    DotAccounting {
                        changed_total: 0,
                        analysis_roots_omitted: 0,
                        deleted_paths_unanalyzed,
                        unmapped_ranges: 0,
                        file_mapped_ranges: 0,
                        traversal_complete,
                    },
                )?,
                evidence: ordered_evidence_text(evidence, static_status),
                static_status,
                dynamic_status,
            });
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
        let mut traversed_ids = roots.iter().map(|root| root.id).collect::<HashSet<_>>();
        let mut calls = ChangeCalls {
            nodes: roots.iter().cloned().map(|node| (node.id, node)).collect(),
            ..ChangeCalls::default()
        };
        let mut evidence_node_ids = traversed_ids.clone();
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
                &mut traversed_ids,
                &mut calls,
            )?
        };
        for flow in &analysis.flows {
            traversed_ids.insert(flow.entry.id);
            traversed_ids.extend(flow.nodes.iter().map(|node| node.id));
            evidence_node_ids.insert(flow.entry.id);
            evidence_node_ids.extend(flow.nodes.iter().map(|node| node.id));
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
        let traversal_complete =
            !analysis_incomplete && !neighborhood_omitted && changed_symbols_omitted == 0;
        let mapping_complete = unmapped_range_count == 0;
        let mut evidence_scope = CoverageScope::changes(changes, dependency_mode, &tx)?;
        evidence_scope.add_nodes(&tx, &evidence_node_ids)?;
        let evidence = render_evidence(&tx, Some(&evidence_scope))?;
        let accounting = static_accounting(&tx, &evidence_scope)?;
        let content_complete = change_content_complete(changes, dependency_mode);
        lines.insert(
            0,
            assurance_preamble(
                &accounting,
                content_complete,
                mapping_complete,
                traversal_complete,
                &evidence,
            ),
        );
        lines.insert(
            0,
            format!(
                "risk overall={} changed_symbols_total={} changed_symbols_analyzed={} changed_symbols_emitted={} changed_symbols_omitted={} {} static_test_path_gaps={} traversal_complete={} analysis_roots_omitted={} deleted_paths_unanalyzed={} neighborhood_omitted={} unmapped_ranges={} file_mapped_ranges={} dependency_analysis={} {}\n",
                score_text(overall),
                changed_symbols_total,
                analysis.risks.len(),
                changed_symbols_emitted,
                changed_symbols_omitted,
                flow_accounting,
                static_test_path_gaps,
                traversal_complete,
                analysis_roots_omitted,
                deleted_paths_unanalyzed,
                neighborhood_omitted,
                unmapped_range_count,
                file_mapped_range_count,
                dependency_analysis(dependency_mode),
                risk_metadata(top_risk),
            ),
        );
        let static_status =
            accounting.overall(content_complete, mapping_complete, traversal_complete);
        let dynamic_status = evidence.status;
        let dot = change_dot(
            snapshot_id,
            &roots,
            &analysis,
            &calls,
            (depth, max_nodes),
            dependency_mode,
            DotAccounting {
                changed_total: changed_symbols_total,
                analysis_roots_omitted,
                deleted_paths_unanalyzed,
                unmapped_ranges: unmapped_range_count,
                file_mapped_ranges: file_mapped_range_count,
                traversal_complete,
            },
        )?;
        Ok(ChangeReview {
            graph: lines.concat(),
            dot,
            evidence: ordered_evidence_text(evidence, static_status),
            static_status,
            dynamic_status,
        })
    }
}

pub fn validate_image(path: &Path) -> Result<State> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect database {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err("database image is not a regular file".into());
    }
    require_no_sidecars(path)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
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
    load_stored_files(&connection)?;
    require_graph_invariants(&connection, &AtomicBool::new(false))?;
    require_evidence_invariants(&connection, &AtomicBool::new(false))?;
    require_no_sidecars(path)?;
    Ok(state)
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

fn require_graph_invariants(connection: &Connection, cancelled: &AtomicBool) -> Result<()> {
    check_cancelled(cancelled)?;
    for state in distinct_text(connection, "SELECT DISTINCT resolution_state FROM refs")? {
        ResolutionState::parse(&state)
            .ok_or_else(|| "database reference resolution is invalid".to_owned())?;
    }
    for category in distinct_text(connection, "SELECT DISTINCT category FROM graph_gaps")? {
        GapCategory::parse(&category)
            .ok_or_else(|| "database gap category is invalid".to_owned())?;
    }
    for reason in distinct_text(connection, "SELECT DISTINCT reason FROM graph_gaps")? {
        GapReason::parse(&reason).ok_or_else(|| "database gap reason is invalid".to_owned())?;
    }
    for kind in distinct_text(connection, "SELECT DISTINCT kind FROM modeled_sites")? {
        ModeledSiteKind::parse(&kind)
            .ok_or_else(|| "database modeled-site kind is invalid".to_owned())?;
    }
    let pending: i64 = connection
        .query_row(
            "SELECT count(*) FROM refs WHERE resolution_state='pending'",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if pending != 0 {
        return Err("database contains pending references".into());
    }
    let invalid: i64 = connection
        .query_row(
            "SELECT count(*) FROM refs
              WHERE (resolution_state='resolved') != (resolved_target_id IS NOT NULL)",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if invalid != 0 {
        return Err("database reference resolution is invalid".into());
    }
    require_reference_candidates(connection, cancelled)?;
    check_cancelled(cancelled)?;
    let invalid_edges: bool = connection
        .query_row(
            "WITH expected AS (
                 SELECT r.source_id, r.resolved_target_id AS target_id,
                        CASE
                            WHEN r.kind='IMPORTS' THEN 'IMPORTS'
                            WHEN n.kind='test' THEN 'TEST_CALLS'
                            ELSE 'CALLS'
                        END AS kind,
                        count(*) AS support_count
                   FROM refs r JOIN nodes n ON n.id=r.source_id
                  WHERE r.resolution_state='resolved'
                  GROUP BY r.source_id, r.resolved_target_id,
                           CASE
                               WHEN r.kind='IMPORTS' THEN 'IMPORTS'
                               WHEN n.kind='test' THEN 'TEST_CALLS'
                               ELSE 'CALLS'
                           END
             )
             SELECT EXISTS(
                 SELECT 1 FROM expected
                 LEFT JOIN edges e
                   ON e.source_id=expected.source_id
                  AND e.target_id=expected.target_id
                  AND e.kind=expected.kind
                WHERE e.support_count IS NULL
                   OR e.support_count!=expected.support_count
                 UNION ALL
                 SELECT 1 FROM edges e
                 LEFT JOIN expected
                   ON expected.source_id=e.source_id
                  AND expected.target_id=e.target_id
                  AND expected.kind=e.kind
                WHERE expected.source_id IS NULL
             )",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if invalid_edges {
        return Err("database reference edges do not match resolved references".into());
    }
    check_cancelled(cancelled)?;
    let mismatch = connection
        .query_row(
            "SELECT f.path FROM files f
              WHERE f.observed_relation_sites !=
                    (SELECT count(*) FROM refs r
                       JOIN nodes n ON n.id=r.source_id WHERE n.file_id=f.id)
                  + (SELECT count(*) FROM modeled_sites m WHERE m.file_id=f.id)
                  + (SELECT coalesce(sum(g.occurrences),0) FROM graph_gaps g
                       LEFT JOIN nodes n ON n.id=g.source_id
                      WHERE g.relation_site=1 AND coalesce(g.file_id,n.file_id)=f.id)
              ORDER BY f.path LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(db_error)?;
    if let Some(path) = mismatch {
        return Err(format!(
            "observed relation-site accounting mismatch for {path}"
        ));
    }
    check_cancelled(cancelled)
}

fn require_evidence_invariants(connection: &Connection, cancelled: &AtomicBool) -> Result<()> {
    check_cancelled(cancelled)?;
    for role in distinct_text(connection, "SELECT DISTINCT role FROM imported_artifacts")? {
        ArtifactRole::parse(&role).ok_or_else(|| "database artifact role is invalid".to_owned())?;
    }
    for path in distinct_text(connection, "SELECT DISTINCT path FROM imported_artifacts")? {
        if !crate::evidence::evidence_path_is_safe(&path) {
            return Err("database evidence artifact path is unsafe".into());
        }
    }
    for format in distinct_text(connection, "SELECT DISTINCT format FROM coverage_runs")? {
        match format.as_str() {
            "llvm" | "coverage_py" => {}
            _ => return Err("database coverage format is invalid".into()),
        }
    }
    for kind in distinct_text(connection, "SELECT DISTINCT kind FROM coverage_branches")? {
        CoverageBranchKind::parse(&kind)
            .ok_or_else(|| "database coverage branch kind is invalid".to_owned())?;
    }
    let mut labels = connection
        .prepare(
            "SELECT run_label FROM coverage_runs
              UNION ALL SELECT test_name FROM coverage_runs WHERE test_name IS NOT NULL
              UNION ALL SELECT context FROM coverage_regions WHERE context IS NOT NULL",
        )
        .map_err(db_error)?;
    for label in labels
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_error)?
    {
        validate_coverage_label(&label.map_err(db_error)?)?;
    }
    require_provenance_invariants(connection, cancelled)?;
    let invalid: bool = connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM provenance_links p
                 JOIN imported_artifacts input ON input.id=p.input_artifact_id
                 JOIN imported_artifacts output ON output.id=p.output_artifact_id
                 LEFT JOIN nodes generator ON generator.id=p.generator_node_id
                 LEFT JOIN files generator_file ON generator_file.id=p.generator_file_id
                 LEFT JOIN modeled_sites site ON site.id=p.modeled_site_id
                 WHERE input.role!='input' OR output.role!='generated-rust'
                    OR (p.generator_file_id IS NULL)!=(p.generator_node_id IS NULL)
                    OR (generator.id IS NOT NULL AND (
                        generator.file_id!=p.generator_file_id
                        OR generator_file.path!=p.generator_path
                        OR generator_file.language NOT IN ('rust','python')
                        OR generator.kind NOT IN ('function','test')
                        OR generator.line_start>p.generator_line_start
                        OR generator.line_end<p.generator_line_end
                    ))
                    OR (site.id IS NOT NULL AND site.kind!='generated-inclusion')
                 UNION ALL
                 SELECT 1 FROM provenance_links
                  WHERE modeled_site_id IS NOT NULL
                  GROUP BY modeled_site_id
                 HAVING count(DISTINCT output_artifact_id)>1
                 UNION ALL
                 SELECT 1 FROM provenance_links
                  WHERE modeled_site_id IS NOT NULL
                  GROUP BY output_artifact_id
                 HAVING count(DISTINCT modeled_site_id)>1
                 UNION ALL
                 SELECT 1 FROM coverage_runs run
                 JOIN imported_artifacts report ON report.id=run.report_artifact_id
                 LEFT JOIN nodes test ON test.id=run.test_id
                 WHERE report.role!='coverage-report'
                    OR report.content_hash!=run.report_digest
                    OR (run.test_name IS NULL AND run.test_id IS NOT NULL)
                    OR (run.test_id IS NOT NULL AND (
                        test.kind!='test'
                        OR (test.name!=run.test_name AND test.qualified_name!=run.test_name)
                    ))
                    OR (run.test_name IS NOT NULL AND run.test_id IS NULL AND NOT EXISTS(
                        SELECT 1 FROM graph_gaps gap
                         WHERE gap.run_id=run.id AND gap.category='coverage'
                           AND gap.reason IN ('missing-test-context','ambiguous-test-context')
                           AND gap.target_hint=run.test_name
                    ))
                 UNION ALL
                 SELECT 1 FROM coverage_regions region
                 JOIN coverage_runs run ON run.id=region.run_id
                 LEFT JOIN files file ON file.id=region.file_id
                 LEFT JOIN nodes test ON test.id=region.test_id
                 WHERE (region.context IS NULL AND (
                            (run.format='llvm' AND region.test_id IS NOT run.test_id)
                            OR (run.format='coverage_py' AND region.test_id IS NOT NULL)
                        ))
                    OR (region.context IS NOT NULL AND region.test_id IS NOT NULL AND (
                        test.kind!='test'
                        OR (test.name!=region.context AND test.qualified_name!=region.context)
                    ))
                    OR (region.context IS NOT NULL AND region.test_id IS NULL AND NOT EXISTS(
                        SELECT 1 FROM graph_gaps gap
                         WHERE gap.run_id=run.id AND gap.category='coverage'
                           AND gap.reason IN ('missing-test-context','ambiguous-test-context')
                           AND gap.target_hint=region.context
                           AND gap.line_start=region.start_line AND gap.line_end=region.end_line
                    ))
                    OR (region.file_id IS NOT NULL AND (
                        region.path IS NULL OR region.path!=file.path
                    ))
                    OR (region.file_id IS NULL AND NOT EXISTS(
                        SELECT 1 FROM graph_gaps gap
                         WHERE gap.run_id=run.id AND gap.category='coverage'
                           AND gap.reason='coverage-unmapped-file'
                           AND (
                               (region.path IS NULL AND gap.path IS NULL
                                AND gap.line_start IS NULL AND gap.line_end IS NULL)
                               OR (region.path IS NOT NULL AND gap.path=region.path
                                   AND gap.line_start=region.start_line
                                   AND gap.line_end=region.end_line)
                           )
                    ))
                    OR (region.file_id IS NOT NULL AND NOT EXISTS(
                        SELECT 1 FROM nodes node
                         WHERE node.file_id=region.file_id AND node.kind!='file'
                           AND node.line_start<=region.end_line
                           AND node.line_end>=region.start_line
                    ) AND NOT EXISTS(
                        SELECT 1 FROM graph_gaps gap
                         WHERE gap.run_id=run.id AND gap.category='coverage'
                           AND gap.reason='coverage-unmapped-region'
                           AND gap.path=file.path
                           AND gap.line_start=region.start_line AND gap.line_end=region.end_line
                    ))
                 UNION ALL
                 SELECT 1 FROM coverage_branches branch
                 JOIN coverage_runs run ON run.id=branch.run_id
                 LEFT JOIN files file ON file.id=branch.file_id
                 LEFT JOIN nodes test ON test.id=branch.test_id
                 WHERE (run.format='llvm' AND branch.test_id IS NOT run.test_id)
                    OR (run.format='coverage_py' AND branch.test_id IS NOT NULL)
                    OR (branch.test_id IS NOT NULL AND test.kind!='test')
                    OR ((branch.kind='arc') != (branch.target_line IS NOT NULL))
                    OR branch.start_line=0
                    OR (branch.kind='arc' AND (
                        branch.end_line!=branch.start_line
                        OR branch.start_column!=0 OR branch.end_column!=0
                        OR branch.target_line=0
                        OR (branch.start_line<0 AND branch.target_line<0)
                    ))
                    OR (branch.kind!='arc' AND (
                        branch.start_line<0 OR branch.end_line<branch.start_line
                        OR (branch.end_line=branch.start_line
                            AND branch.end_column<branch.start_column)
                    ))
                    OR (branch.file_id IS NOT NULL AND (
                        branch.path IS NULL OR branch.path!=file.path
                    ))
                    OR (branch.file_id IS NULL AND NOT EXISTS(
                        SELECT 1 FROM graph_gaps gap
                         WHERE gap.run_id=run.id AND gap.category='coverage'
                           AND gap.reason='coverage-unmapped-file'
                           AND (
                               (branch.path IS NULL AND gap.path IS NULL
                                AND gap.line_start IS NULL AND gap.line_end IS NULL)
                               OR (branch.path IS NOT NULL AND gap.path=branch.path AND (
                                   (branch.kind!='arc'
                                    AND gap.line_start=branch.start_line
                                    AND gap.line_end=branch.end_line)
                                   OR (branch.kind='arc'
                                    AND gap.line_start=CASE
                                        WHEN branch.start_line>0 AND branch.target_line>0
                                            THEN min(branch.start_line,branch.target_line)
                                        WHEN branch.start_line>0 THEN branch.start_line
                                        ELSE branch.target_line END
                                    AND gap.line_end=CASE
                                        WHEN branch.start_line>0 AND branch.target_line>0
                                            THEN max(branch.start_line,branch.target_line)
                                        WHEN branch.start_line>0 THEN branch.start_line
                                        ELSE branch.target_line END)
                               ))
                           )
                    ))
                    OR (branch.file_id IS NOT NULL AND NOT EXISTS(
                        SELECT 1 FROM nodes node
                         WHERE node.file_id=branch.file_id AND node.kind!='file'
                           AND node.line_start<=CASE
                               WHEN branch.kind!='arc' THEN branch.end_line
                               WHEN branch.start_line>0 AND branch.target_line>0
                                   THEN max(branch.start_line,branch.target_line)
                               WHEN branch.start_line>0 THEN branch.start_line
                               ELSE branch.target_line END
                           AND node.line_end>=CASE
                               WHEN branch.kind!='arc' THEN branch.start_line
                               WHEN branch.start_line>0 AND branch.target_line>0
                                   THEN min(branch.start_line,branch.target_line)
                               WHEN branch.start_line>0 THEN branch.start_line
                               ELSE branch.target_line END
                    ) AND NOT EXISTS(
                        SELECT 1 FROM graph_gaps gap
                         WHERE gap.run_id=run.id AND gap.category='coverage'
                           AND gap.reason='coverage-unmapped-region'
                           AND gap.path=file.path
                           AND gap.line_start=CASE
                               WHEN branch.kind!='arc' THEN branch.start_line
                               WHEN branch.start_line>0 AND branch.target_line>0
                                   THEN min(branch.start_line,branch.target_line)
                               WHEN branch.start_line>0 THEN branch.start_line
                               ELSE branch.target_line END
                           AND gap.line_end=CASE
                               WHEN branch.kind!='arc' THEN branch.end_line
                               WHEN branch.start_line>0 AND branch.target_line>0
                                   THEN max(branch.start_line,branch.target_line)
                               WHEN branch.start_line>0 THEN branch.start_line
                               ELSE branch.target_line END
                    ))
                 UNION ALL
                 SELECT 1 FROM graph_gaps gap
                 WHERE (gap.category='coverage') != (gap.run_id IS NOT NULL)
                    OR (gap.category='coverage' AND gap.reason NOT IN (
                        'coverage-unmapped-file','coverage-unmapped-region',
                        'missing-test-context','ambiguous-test-context'
                    ))
                 UNION ALL
                 SELECT 1 FROM files file JOIN imported_artifacts artifact
                   ON artifact.role='generated-rust' AND artifact.path=file.path
                 WHERE artifact.content_hash!=file.content_hash
                    OR artifact.byte_size!=file.byte_size
                 UNION ALL
                 SELECT 1 FROM provenance_links link
                 JOIN imported_artifacts output ON output.id=link.output_artifact_id
                 LEFT JOIN files file ON file.path=output.path
                 WHERE link.modeled_site_id IS NOT NULL AND file.id IS NULL
             )",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if invalid {
        return Err("database evidence relationships are invalid".into());
    }
    check_cancelled(cancelled)
}

struct StoredProvenance {
    input_id: i64,
    input_role: Option<String>,
    input_lines: EvidenceLineSpan,
    generator_path: String,
    generator_file_id: Option<i64>,
    generator_node_id: Option<i64>,
    generator_lines: EvidenceLineSpan,
    output_id: i64,
    output_path: Option<String>,
    output_role: Option<String>,
    output_lines: EvidenceLineSpan,
    modeled_site_id: Option<i64>,
    mapping_state: String,
}

fn require_provenance_invariants(connection: &Connection, cancelled: &AtomicBool) -> Result<()> {
    let rows = connection
        .prepare(
            "SELECT p.input_artifact_id, input.role,
                    p.input_line_start, p.input_line_end,
                    p.generator_path, p.generator_file_id, p.generator_node_id,
                    p.generator_line_start, p.generator_line_end,
                    p.output_artifact_id, output.path, output.role,
                    p.output_line_start, p.output_line_end,
                    p.modeled_site_id, p.mapping_state
               FROM provenance_links p
               LEFT JOIN imported_artifacts input ON input.id=p.input_artifact_id
               LEFT JOIN imported_artifacts output ON output.id=p.output_artifact_id
              ORDER BY p.id",
        )
        .map_err(db_error)?
        .query_map([], |row| {
            Ok(StoredProvenance {
                input_id: row.get(0)?,
                input_role: row.get(1)?,
                input_lines: EvidenceLineSpan {
                    start: row.get(2)?,
                    end: row.get(3)?,
                },
                generator_path: row.get(4)?,
                generator_file_id: row.get(5)?,
                generator_node_id: row.get(6)?,
                generator_lines: EvidenceLineSpan {
                    start: row.get(7)?,
                    end: row.get(8)?,
                },
                output_id: row.get(9)?,
                output_path: row.get(10)?,
                output_role: row.get(11)?,
                output_lines: EvidenceLineSpan {
                    start: row.get(12)?,
                    end: row.get(13)?,
                },
                modeled_site_id: row.get(14)?,
                mapping_state: row.get(15)?,
            })
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    let mut paths_by_basename = BTreeMap::<String, BTreeSet<String>>::new();
    for row in &rows {
        let output_path = row
            .output_path
            .as_deref()
            .ok_or_else(|| "database provenance output artifact is missing".to_owned())?;
        let basename = Path::new(output_path)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "database provenance output basename is invalid".to_owned())?;
        paths_by_basename
            .entry(basename.to_owned())
            .or_default()
            .insert(output_path.to_owned());
    }
    let mut identities = HashSet::new();
    for row in rows {
        check_cancelled(cancelled)?;
        validate_evidence_span(row.input_lines)?;
        validate_evidence_span(row.generator_lines)?;
        validate_evidence_span(row.output_lines)?;
        if row.input_role.as_deref() != Some(ArtifactRole::Input.db())
            || row.output_role.as_deref() != Some(ArtifactRole::GeneratedRust.db())
        {
            return Err("database provenance artifact role is invalid".into());
        }
        if !crate::evidence::evidence_path_is_safe(&row.generator_path) {
            return Err("database provenance declaration path is unsafe".into());
        }
        if !identities.insert((
            row.input_id,
            row.input_lines.start,
            row.input_lines.end,
            row.generator_path.clone(),
            row.generator_lines.start,
            row.generator_lines.end,
            row.output_id,
            row.output_lines.start,
            row.output_lines.end,
        )) {
            return Err("database provenance declaration identity is duplicated".into());
        }
        let output_path = row
            .output_path
            .as_deref()
            .ok_or_else(|| "database provenance output artifact is missing".to_owned())?;
        let basename = Path::new(output_path)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "database provenance output basename is invalid".to_owned())?;
        let contended = paths_by_basename
            .get(basename)
            .is_some_and(|paths| paths.len() > 1);
        let resolution = provenance_resolution(
            connection,
            &row.generator_path,
            row.generator_lines,
            basename,
            contended,
        )?;
        if row.generator_file_id != resolution.generator_file_id
            || row.generator_node_id != resolution.generator_node_id
            || row.modeled_site_id != resolution.modeled_site_id
            || row.mapping_state != resolution.mapping_state
        {
            return Err("database provenance declaration mapping is inconsistent".into());
        }
        if row.modeled_site_id.is_some() {
            let generated_file: bool = connection
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM files file JOIN imported_artifacts output
                           ON output.id=?1 AND output.path=file.path
                          AND output.content_hash=file.content_hash
                          AND output.byte_size=file.byte_size
                     )",
                    [row.output_id],
                    |result| result.get(0),
                )
                .map_err(db_error)?;
            if !generated_file {
                return Err("database provenance generated output is missing".into());
            }
        }
    }
    Ok(())
}

fn require_reference_candidates(connection: &Connection, cancelled: &AtomicBool) -> Result<()> {
    let mut load_references = connection
        .prepare(
            "SELECT id, alias_key, resolved_target_id, resolution_state
               FROM refs ORDER BY id",
        )
        .map_err(db_error)?;
    let rows = load_references
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(db_error)?;
    let mut references = Vec::new();
    for row in rows {
        check_cancelled(cancelled)?;
        references.push(row.map_err(db_error)?);
    }
    let mut load_keys = connection
        .prepare("SELECT key FROM ref_keys WHERE ref_id=?1 ORDER BY rank")
        .map_err(db_error)?;
    let mut candidates = connection
        .prepare("SELECT node_id FROM node_keys WHERE key=?1 ORDER BY node_id LIMIT 2")
        .map_err(db_error)?;
    let mut loaded = Vec::with_capacity(references.len());
    let mut aliases = HashMap::new();
    for (id, alias, target, state) in references {
        check_cancelled(cancelled)?;
        let rows = load_keys
            .query_map([id], |row| row.get::<_, String>(0))
            .map_err(db_error)?;
        let mut keys = Vec::new();
        for row in rows {
            check_cancelled(cancelled)?;
            keys.push(row.map_err(db_error)?);
        }
        let direct = expected_reference_candidate(&keys, &mut candidates, None, cancelled)?;
        if let Some(alias) = &alias
            && !matches!(direct, DbCandidate::Missing)
        {
            aliases
                .entry(alias.clone())
                .and_modify(|candidate| {
                    *candidate = match (*candidate, direct) {
                        (DbCandidate::Unique(left), DbCandidate::Unique(right))
                            if left == right =>
                        {
                            DbCandidate::Unique(left)
                        }
                        _ => DbCandidate::Ambiguous,
                    };
                })
                .or_insert(direct);
        }
        loaded.push((alias, target, state, keys, direct));
    }

    #[cfg(test)]
    AFTER_REFERENCE_CANDIDATE_PASS_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    check_cancelled(cancelled)?;

    for (alias, actual_target, actual_state, keys, direct) in loaded {
        check_cancelled(cancelled)?;
        let expected = if alias.is_some() {
            direct
        } else {
            expected_reference_candidate(&keys, &mut candidates, Some(&aliases), cancelled)?
        };
        let (expected_target, expected_state) = match expected {
            DbCandidate::Unique(target) => (Some(target), ResolutionState::Resolved),
            DbCandidate::Missing => (None, ResolutionState::Missing),
            DbCandidate::Ambiguous => (None, ResolutionState::Ambiguous),
        };
        if actual_target != expected_target || actual_state != expected_state.db() {
            return Err("database reference resolution does not match candidates".into());
        }
    }
    Ok(())
}

fn expected_reference_candidate(
    keys: &[String],
    candidates: &mut rusqlite::Statement<'_>,
    aliases: Option<&HashMap<String, DbCandidate>>,
    cancelled: &AtomicBool,
) -> Result<DbCandidate> {
    for key in keys {
        check_cancelled(cancelled)?;
        let direct = candidate(candidates, key)?;
        let alias = aliases
            .and_then(|aliases| aliases.get(key))
            .copied()
            .unwrap_or(DbCandidate::Missing);
        match merge_candidates(direct, alias) {
            DbCandidate::Unique(target) => return Ok(DbCandidate::Unique(target)),
            DbCandidate::Ambiguous => return Ok(DbCandidate::Ambiguous),
            DbCandidate::Missing => {}
        }
    }
    Ok(DbCandidate::Missing)
}

fn distinct_text(connection: &Connection, sql: &str) -> Result<Vec<String>> {
    connection
        .prepare(sql)
        .map_err(db_error)?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)
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

#[derive(Clone)]
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

#[derive(Default)]
struct ChangeCalls {
    nodes: HashMap<i64, RowNode>,
    // (caller, callee, is_test_call)
    edges: BTreeSet<(i64, i64, bool)>,
}

#[derive(Clone, Copy)]
struct DotAccounting {
    changed_total: usize,
    analysis_roots_omitted: usize,
    deleted_paths_unanalyzed: usize,
    unmapped_ranges: usize,
    file_mapped_ranges: usize,
    traversal_complete: bool,
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
    visited: &mut HashSet<i64>,
    calls: &mut ChangeCalls,
) -> Result<bool> {
    let (depth, max_nodes) = limits;
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
                    match relation {
                        "test <-" => {
                            calls.nodes.entry(node.id).or_insert_with(|| node.clone());
                            calls.edges.insert((node.id, source, true));
                        }
                        "caller <-" => {
                            calls.nodes.entry(node.id).or_insert_with(|| node.clone());
                            calls.edges.insert((node.id, source, false));
                        }
                        _ => {}
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

struct StaticAccounting {
    source_complete: bool,
    syntax_complete: bool,
    static_model_complete: bool,
    total_gaps: u64,
    relevant_gaps: u64,
    gaps_by_reason: BTreeMap<GapReason, u64>,
    gap_records: String,
    missing: u64,
    ambiguous: u64,
}

impl StaticAccounting {
    fn overall(
        &self,
        content_complete: bool,
        mapping_complete: bool,
        traversal_complete: bool,
    ) -> CompletenessStatus {
        if content_complete
            && self.source_complete
            && self.syntax_complete
            && self.static_model_complete
            && mapping_complete
            && traversal_complete
        {
            CompletenessStatus::Complete
        } else {
            CompletenessStatus::Partial
        }
    }
}

fn static_accounting(connection: &Connection, scope: &CoverageScope) -> Result<StaticAccounting> {
    let mut accounting = StaticAccounting {
        source_complete: true,
        syntax_complete: true,
        static_model_complete: true,
        total_gaps: 0,
        relevant_gaps: 0,
        gaps_by_reason: BTreeMap::new(),
        gap_records: String::new(),
        missing: 0,
        ambiguous: 0,
    };
    let mut statement = connection
        .prepare(
            "SELECT category, reason, sum(occurrences)
               FROM graph_gaps
              GROUP BY category, reason
              ORDER BY category, reason",
        )
        .map_err(db_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(db_error)?;
    for row in rows {
        let (category, reason, occurrences) = row.map_err(db_error)?;
        let occurrences = u64::try_from(occurrences)
            .map_err(|_| "database gap occurrence count is invalid".to_owned())?;
        let category = GapCategory::parse(&category)
            .ok_or_else(|| "database gap category is invalid".to_owned())?;
        let reason =
            GapReason::parse(&reason).ok_or_else(|| "database gap reason is invalid".to_owned())?;
        accounting.total_gaps = accounting.total_gaps.saturating_add(occurrences);
        let reason_count = accounting.gaps_by_reason.entry(reason).or_default();
        *reason_count = reason_count.saturating_add(occurrences);
        match category {
            GapCategory::Source => {
                accounting.source_complete = false;
                accounting.static_model_complete = false;
                accounting.relevant_gaps = accounting.relevant_gaps.saturating_add(occurrences);
            }
            GapCategory::Parse => {
                accounting.syntax_complete = false;
                accounting.static_model_complete = false;
                accounting.relevant_gaps = accounting.relevant_gaps.saturating_add(occurrences);
            }
            GapCategory::Coverage => {}
            GapCategory::Relation
            | GapCategory::Macro
            | GapCategory::Generated
            | GapCategory::Boundary => {
                accounting.static_model_complete = false;
                accounting.relevant_gaps = accounting.relevant_gaps.saturating_add(occurrences);
            }
            GapCategory::Language => {
                accounting.source_complete = false;
                accounting.static_model_complete = false;
                accounting.relevant_gaps = accounting.relevant_gaps.saturating_add(occurrences);
            }
        }
    }
    accounting.gap_records = static_gap_records(connection, scope)?;
    let (missing, ambiguous) = connection
        .query_row(
            "SELECT
                sum(CASE WHEN resolution_state='missing' THEN 1 ELSE 0 END),
                sum(CASE WHEN resolution_state='ambiguous' THEN 1 ELSE 0 END)
               FROM refs",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or_default(),
                    row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .map_err(db_error)?;
    accounting.missing = u64::try_from(missing)
        .map_err(|_| "database missing reference count is invalid".to_owned())?;
    accounting.ambiguous = u64::try_from(ambiguous)
        .map_err(|_| "database ambiguous reference count is invalid".to_owned())?;
    let mut relevant_keys = HashSet::new();
    let mut load_keys = connection
        .prepare("SELECT key FROM node_keys WHERE node_id=?1 ORDER BY key")
        .map_err(db_error)?;
    let mut relevant_nodes = scope.nodes.iter().copied().collect::<Vec<_>>();
    relevant_nodes.sort_unstable();
    for node_id in relevant_nodes {
        for row in load_keys
            .query_map([node_id], |row| row.get::<_, String>(0))
            .map_err(db_error)?
        {
            relevant_keys.insert(row.map_err(db_error)?);
        }
    }
    let relevant_unresolved = if relevant_keys.is_empty() {
        false
    } else {
        let mut unresolved = connection
            .prepare(
                "SELECT key.key FROM refs ref JOIN ref_keys key ON key.ref_id=ref.id
                  WHERE ref.resolution_state IN ('missing','ambiguous')
                  ORDER BY ref.id, key.rank",
            )
            .map_err(db_error)?;
        let mut rows = unresolved.query([]).map_err(db_error)?;
        let mut relevant = false;
        while let Some(row) = rows.next().map_err(db_error)? {
            if relevant_keys.contains(&row.get::<_, String>(0).map_err(db_error)?) {
                relevant = true;
                break;
            }
        }
        relevant
    };
    if relevant_unresolved {
        accounting.static_model_complete = false;
    }
    Ok(accounting)
}

fn static_gap_records(connection: &Connection, scope: &CoverageScope) -> Result<String> {
    let mut statement = connection
        .prepare(
            "SELECT category, reason, path, line_start, line_end, target_hint,
                    occurrences, relation_site, id, source_id
               FROM graph_gaps
              WHERE category NOT IN ('coverage','generated')",
        )
        .map_err(db_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<u32>>(3)?,
                row.get::<_, Option<u32>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, bool>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<i64>>(9)?,
            ))
        })
        .map_err(db_error)?;
    let mut gaps = Vec::new();
    for row in rows {
        let (category, reason, path, start, end, target, occurrences, relation_site, id, source_id) =
            row.map_err(db_error)?;
        let relevant = scope.gap_relevant(path.as_deref(), start, end, source_id);
        if !relevant {
            continue;
        }
        gaps.push((
            GapReason::parse(&reason).ok_or_else(|| "database gap reason is invalid".to_owned())?,
            path,
            start,
            end,
            GapCategory::parse(&category)
                .ok_or_else(|| "database gap category is invalid".to_owned())?,
            target,
            occurrences,
            relation_site,
            id,
        ));
    }
    gaps.sort_unstable();

    let mut text = String::new();
    for (reason, path, start, end, category, target, occurrences, relation_site, _) in gaps {
        let line = match (start, end) {
            (Some(start), Some(end)) => line_range(start, end),
            (Some(start), None) => start.to_string(),
            (None, _) => "none".into(),
        };
        text.push_str(&format!(
            "gap category={} reason={} path={:?} line={line}",
            category.db(),
            reason.db(),
            path.unwrap_or_default(),
        ));
        if let Some(target) = target {
            text.push_str(&format!(" target={target:?}"));
        }
        text.push_str(&format!(
            " occurrences={occurrences} relation_site={relation_site}\n"
        ));
    }
    Ok(text)
}

#[derive(Default)]
struct CoverageScope {
    ranges: BTreeMap<String, Option<Vec<(u32, u32)>>>,
    nodes: HashSet<i64>,
}

impl CoverageScope {
    fn node(node_id: i64, path: &str, start: u32, end: u32, whole_file: bool) -> Self {
        let mut scope = Self::default();
        scope.nodes.insert(node_id);
        if whole_file {
            scope.ranges.insert(path.to_owned(), None);
        } else {
            scope.add_range(path, start, end);
        }
        scope
    }

    fn changes(
        changes: &WorktreeChanges,
        dependency_mode: DependencyMode,
        connection: &Connection,
    ) -> Result<Self> {
        let mut scope = Self::default();
        for file in &changes.files {
            if dependency_mode == DependencyMode::Boundary
                && dependency_package(&file.path).is_some()
            {
                continue;
            }
            if file.whole_file {
                scope.ranges.insert(file.path.clone(), None);
                continue;
            }
            for span in &file.spans {
                let start = u32::try_from((span.start / 2).max(1)).unwrap_or(u32::MAX);
                let end = u32::try_from((span.end / 2).max(u64::from(start))).unwrap_or(u32::MAX);
                scope.add_range(&file.path, start, end);
            }
        }
        for path in &changes.paths {
            if path.language.is_none()
                && (dependency_mode == DependencyMode::Full
                    || dependency_package(&path.path).is_none())
            {
                scope.ranges.entry(path.path.clone()).or_insert(None);
            }
        }
        let mut nodes = connection
            .prepare(
                "SELECT node.id, node.line_start, node.line_end
                   FROM nodes node JOIN files file ON file.id=node.file_id
                  WHERE file.path=?1
                  ORDER BY node.line_start, node.line_end, node.id",
            )
            .map_err(db_error)?;
        for file in &changes.files {
            if dependency_mode == DependencyMode::Boundary
                && dependency_package(&file.path).is_some()
            {
                continue;
            }
            let rows = nodes
                .query_map([&file.path], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                    ))
                })
                .map_err(db_error)?;
            for row in rows {
                let (id, start, end) = row.map_err(db_error)?;
                let node_start = u64::from(start).saturating_mul(2);
                let node_end = u64::from(end).saturating_mul(2);
                if file.whole_file
                    || file
                        .spans
                        .iter()
                        .any(|span| span.start <= node_end && span.end >= node_start)
                {
                    scope.nodes.insert(id);
                    scope.add_range(&file.path, start, end);
                }
            }
        }
        scope.expand_provenance(connection)?;
        Ok(scope)
    }

    fn add_nodes(&mut self, connection: &Connection, node_ids: &HashSet<i64>) -> Result<()> {
        let mut statement = connection
            .prepare(
                "SELECT file.path, node.line_start, node.line_end
                   FROM nodes node JOIN files file ON file.id=node.file_id
                  WHERE node.id=?1",
            )
            .map_err(db_error)?;
        let mut node_ids = node_ids.iter().copied().collect::<Vec<_>>();
        node_ids.sort_unstable();
        for node_id in node_ids {
            if let Some((path, start, end)) = statement
                .query_row([node_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                    ))
                })
                .optional()
                .map_err(db_error)?
            {
                self.nodes.insert(node_id);
                self.add_range(&path, start, end);
            }
        }
        self.expand_provenance(connection)
    }

    fn expand_provenance(&mut self, connection: &Connection) -> Result<()> {
        type Row = (
            String,
            u32,
            u32,
            String,
            u32,
            u32,
            String,
            u32,
            u32,
            Option<String>,
            Option<u32>,
            Option<i64>,
            Option<i64>,
        );
        let rows = connection
            .prepare(
                "SELECT input.path, link.input_line_start, link.input_line_end,
                        output.path, link.output_line_start, link.output_line_end,
                        link.generator_path, link.generator_line_start, link.generator_line_end,
                        include_file.path, site.line_start,
                        link.generator_node_id, site.source_id
                   FROM provenance_links link
                   JOIN imported_artifacts input ON input.id=link.input_artifact_id
                   JOIN imported_artifacts output ON output.id=link.output_artifact_id
                   LEFT JOIN modeled_sites site ON site.id=link.modeled_site_id
                   LEFT JOIN files include_file ON include_file.id=site.file_id
                  ORDER BY link.id",
            )
            .map_err(db_error)?
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<Row>>>()
            .map_err(db_error)?;
        loop {
            let mut changed = false;
            for (
                input,
                input_start,
                input_end,
                output,
                output_start,
                output_end,
                generator,
                generator_start,
                generator_end,
                include,
                include_line,
                generator_node_id,
                include_source_id,
            ) in &rows
            {
                let relevant = self.relevant(input, *input_start, *input_end)
                    || self.relevant(output, *output_start, *output_end)
                    || self.relevant(generator, *generator_start, *generator_end)
                    || include
                        .as_deref()
                        .zip(*include_line)
                        .is_some_and(|(path, line)| self.relevant(path, line, line));
                if relevant {
                    changed |= self.add_range(input, *input_start, *input_end);
                    changed |= self.add_range(output, *output_start, *output_end);
                    changed |= self.add_range(generator, *generator_start, *generator_end);
                    if let Some(node_id) = generator_node_id {
                        changed |= self.nodes.insert(*node_id);
                    }
                    if let Some(node_id) = include_source_id {
                        changed |= self.nodes.insert(*node_id);
                    }
                    if let Some((include, line)) = include.as_deref().zip(*include_line) {
                        changed |= self.add_range(include, line, line);
                    }
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    fn add_range(&mut self, path: &str, start: u32, end: u32) -> bool {
        match self.ranges.entry(path.to_owned()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(vec![(start, end)]));
                true
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if let Some(ranges) = entry.get_mut() {
                    if ranges.contains(&(start, end)) {
                        return false;
                    }
                    ranges.push((start, end));
                    true
                } else {
                    false
                }
            }
        }
    }

    fn whole_file(&self, path: &str) -> bool {
        self.ranges.get(path).is_some_and(Option::is_none)
    }

    fn gap_relevant(
        &self,
        path: Option<&str>,
        start: Option<u32>,
        end: Option<u32>,
        source_id: Option<i64>,
    ) -> bool {
        if path.is_some_and(|path| self.whole_file(path)) {
            return true;
        }
        source_id.is_some_and(|source_id| self.nodes.contains(&source_id))
            || path.is_some_and(|path| match start {
                Some(start) => self.relevant(path, start, end.unwrap_or(start)),
                None => false,
            })
    }

    fn relevant(&self, path: &str, start: u32, end: u32) -> bool {
        match self.ranges.get(path) {
            Some(None) => true,
            Some(Some(ranges)) => ranges
                .iter()
                .any(|(scope_start, scope_end)| *scope_start <= end && *scope_end >= start),
            None => false,
        }
    }

    fn relevant_branch(
        &self,
        path: &str,
        start: i64,
        end: i64,
        target: Option<i64>,
        kind: &str,
    ) -> bool {
        if kind != "arc" {
            return match (u32::try_from(start), u32::try_from(end)) {
                (Ok(start), Ok(end)) => self.relevant(path, start, end),
                _ => false,
            };
        }
        [Some(start), target]
            .into_iter()
            .flatten()
            .filter_map(|line| u32::try_from(line).ok())
            .any(|line| self.relevant(path, line, line))
    }
}

struct EvidenceReview {
    text: String,
    status: CompletenessStatus,
    capture_status: &'static str,
    provenance_status: &'static str,
    execution_status: &'static str,
}

struct CoverageOutput {
    run: String,
    path: String,
    start: i64,
    start_column: u32,
    end: i64,
    end_column: u32,
    named: bool,
    kind: String,
    block: String,
}

fn render_evidence(
    connection: &Connection,
    scope: Option<&CoverageScope>,
) -> Result<EvidenceReview> {
    let manifest_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM imported_artifacts WHERE role='manifest'",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if manifest_count == 0 {
        return Ok(EvidenceReview {
            text: String::new(),
            status: CompletenessStatus::NotApplicable,
            capture_status: "not-applicable",
            provenance_status: "not-applicable",
            execution_status: "not-applicable",
        });
    }
    if manifest_count != 1 {
        return Err("database manifest evidence identity is invalid".into());
    }
    let mut statement = connection
        .prepare(
            "SELECT output.path,
                    input.path, p.input_line_start, p.input_line_end,
                    p.generator_path, p.generator_line_start, p.generator_line_end,
                    p.output_line_start, p.output_line_end,
                    include_file.path, site.line_start,
                    generated_file.id, p.mapping_state
               FROM provenance_links p
               JOIN imported_artifacts output ON output.id=p.output_artifact_id
               JOIN imported_artifacts input ON input.id=p.input_artifact_id
               LEFT JOIN modeled_sites site ON site.id=p.modeled_site_id
               LEFT JOIN files include_file ON include_file.id=site.file_id
               LEFT JOIN files generated_file
                 ON generated_file.path=output.path
                AND generated_file.content_hash=output.content_hash
                AND generated_file.byte_size=output.byte_size
              ORDER BY output.path, p.id",
        )
        .map_err(db_error)?;
    type ProvenanceRow = (
        String,
        String,
        u32,
        u32,
        String,
        u32,
        u32,
        u32,
        u32,
        Option<String>,
        Option<u32>,
        Option<i64>,
        String,
    );
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
                row.get(12)?,
            ))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<ProvenanceRow>>>()
        .map_err(db_error)?;
    let rows = rows
        .into_iter()
        .filter(|row| {
            scope.is_none_or(|scope| {
                scope.relevant(&row.1, row.2, row.3)
                    || scope.relevant(&row.4, row.5, row.6)
                    || scope.relevant(&row.0, row.7, row.8)
                    || row
                        .9
                        .as_deref()
                        .zip(row.10)
                        .is_some_and(|(path, line)| scope.relevant(path, line, line))
            })
        })
        .collect::<Vec<_>>();
    type GeneratedGapRow = (
        String,
        Option<String>,
        Option<u32>,
        Option<u32>,
        Option<String>,
        u32,
        Option<i64>,
    );
    let generated_gaps = connection
        .prepare(
            "SELECT reason, path, line_start, line_end, target_hint, occurrences,
                    source_id
               FROM graph_gaps WHERE category='generated'
              ORDER BY path, line_start, line_end, reason, id",
        )
        .map_err(db_error)?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<GeneratedGapRow>>>()
        .map_err(db_error)?
        .into_iter()
        .filter(|(_, path, start, end, _, _, source_id)| {
            scope.is_none_or(|scope| scope.gap_relevant(path.as_deref(), *start, *end, *source_id))
        })
        .collect::<Vec<_>>();
    let provenance_applicable = !rows.is_empty();
    let provenance_complete = provenance_applicable
        && rows
            .iter()
            .all(|row| row.12 == "linked" && row.11.is_some());
    let provenance_status = if !provenance_applicable {
        "not-applicable"
    } else if provenance_complete {
        "complete"
    } else {
        "partial"
    };
    let coverage_runs: i64 = connection
        .query_row("SELECT count(*) FROM coverage_runs", [], |row| row.get(0))
        .map_err(db_error)?;
    let (coverage_claims, coverage_gaps) = coverage_relevance(connection, scope)?;
    let execution_status = if coverage_runs == 0 || coverage_claims == 0 && coverage_gaps == 0 {
        "not-applicable"
    } else if coverage_gaps == 0 {
        "complete"
    } else {
        "partial"
    };
    let mut text = format!(
        "completeness evidence_capture=complete provenance_model={} execution_mapping={execution_status}\n",
        provenance_status
    );
    for row in rows {
        let complete = row.12 == "linked" && row.11.is_some();
        let input = format!("{}:{}-{}", row.1, row.2, row.3);
        let generator = format!("{}:{}-{}", row.4, row.5, row.6);
        let output = format!("{}:{}-{}", row.0, row.7, row.8);
        text.push_str(&format!(
            "claim kind=generated-provenance status={} result={} basis=verified-generated-manifest input={input:?} generator={generator:?} output={output:?}\n",
            if complete { "complete" } else { "partial" },
            if complete { "linked" } else { "unknown" },
        ));
        text.push_str(&format!(
            "provenance input={input:?} generator={generator:?} output={output:?}\n"
        ));
        if let (Some(source), Some(line)) = (row.9, row.10) {
            text.push_str(&format!(
                "includes source={:?} output={:?}\n",
                format!("{source}:{line}"),
                row.0,
            ));
        }
        if !complete {
            let reason = if row.12 == "ambiguous" {
                "ambiguous"
            } else {
                "unobserved"
            };
            text.push_str(&format!(
                "gap category=generated reason=generated-output-{reason} input={input:?} generator={generator:?} output={output:?} occurrences=1\n"
            ));
        }
    }
    for (reason, path, start, end, target, occurrences, _) in generated_gaps {
        let line = match (start, end) {
            (Some(start), Some(end)) if start != end => format!("{start}-{end}"),
            (Some(start), _) => start.to_string(),
            _ => "none".into(),
        };
        text.push_str(&format!(
            "gap category=generated reason={reason} path={:?} line={line}",
            path.unwrap_or_default(),
        ));
        if let Some(target) = target {
            text.push_str(&format!(" target={target:?}"));
        }
        text.push_str(&format!(" occurrences={occurrences}\n"));
    }
    render_coverage_observations(connection, scope, &mut text)?;
    Ok(EvidenceReview {
        text,
        status: if provenance_status != "partial" && execution_status != "partial" {
            CompletenessStatus::Complete
        } else {
            CompletenessStatus::Partial
        },
        capture_status: "complete",
        provenance_status,
        execution_status,
    })
}

fn coverage_relevance(
    connection: &Connection,
    scope: Option<&CoverageScope>,
) -> Result<(usize, usize)> {
    let mut regions = connection
        .prepare(
            "SELECT region.run_id, file.path, region.start_line, region.end_line
               FROM coverage_regions region JOIN files file ON file.id=region.file_id
              ORDER BY region.id",
        )
        .map_err(db_error)?;
    let rows = regions
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, u32>(3)?,
            ))
        })
        .map_err(db_error)?;
    let mut claim_count = 0;
    let mut relevant_runs = HashSet::new();
    for row in rows {
        let (run_id, path, start, end) = row.map_err(db_error)?;
        if scope.is_none_or(|scope| scope.relevant(&path, start, end)) {
            claim_count += 1;
            relevant_runs.insert(run_id);
        }
    }
    let mut branches = connection
        .prepare(
            "SELECT branch.run_id, file.path, branch.start_line, branch.end_line,
                    branch.target_line, branch.kind
               FROM coverage_branches branch JOIN files file ON file.id=branch.file_id
              ORDER BY branch.id",
        )
        .map_err(db_error)?;
    let rows = branches
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(db_error)?;
    for row in rows {
        let (run_id, path, start, end, target, kind) = row.map_err(db_error)?;
        if scope.is_none_or(|scope| scope.relevant_branch(&path, start, end, target, &kind)) {
            claim_count += 1;
            relevant_runs.insert(run_id);
        }
    }
    let mut gaps = connection
        .prepare(
            "SELECT run_id, reason, path, line_start, line_end
               FROM graph_gaps WHERE category='coverage'",
        )
        .map_err(db_error)?;
    let rows = gaps
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<u32>>(3)?,
                row.get::<_, Option<u32>>(4)?,
            ))
        })
        .map_err(db_error)?;
    let mut gap_count = 0;
    for row in rows {
        let (run_id, reason, path, start, end) = row.map_err(db_error)?;
        let relevant = match scope {
            None => true,
            Some(scope) => match path.as_deref() {
                Some(path) => scope.relevant(path, start.unwrap_or(1), end.unwrap_or(u32::MAX)),
                None => reason == "coverage-unmapped-file" || relevant_runs.contains(&run_id),
            },
        };
        if relevant {
            gap_count += 1;
        }
    }
    Ok((claim_count, gap_count))
}

fn render_coverage_observations(
    connection: &Connection,
    scope: Option<&CoverageScope>,
    text: &mut String,
) -> Result<()> {
    let mut observations = Vec::new();
    let mut relevant_runs = HashSet::new();
    let mut regions = connection
        .prepare(
            "SELECT run.run_label, run.format, file.path,
                    region.start_line, region.start_column,
                    region.end_line, region.end_column, region.execution_count,
                    CASE WHEN region.test_id IS NOT NULL
                         THEN coalesce(region.context, run.test_name) END,
                    NOT EXISTS(
                        SELECT 1 FROM graph_gaps gap
                         WHERE gap.run_id=run.id AND gap.category='coverage' AND (
                            (gap.reason IN ('missing-test-context','ambiguous-test-context')
                             AND gap.target_hint=CASE WHEN run.format='llvm'
                                 THEN run.test_name ELSE region.context END)
                            OR (gap.reason IN ('coverage-unmapped-file','coverage-unmapped-region')
                                AND gap.path=file.path
                                AND gap.line_start<=region.end_line
                                AND gap.line_end>=region.start_line)
                         )
                    ), run.id
               FROM coverage_regions region
               JOIN coverage_runs run ON run.id=region.run_id
               JOIN files file ON file.id=region.file_id
              ORDER BY (region.test_id IS NULL), run.run_label, file.path,
                       region.start_line, region.start_column, region.end_line,
                       region.end_column, region.context, region.id",
        )
        .map_err(db_error)?;
    let rows = regions
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u32>(3)?,
                row.get::<_, u32>(4)?,
                row.get::<_, u32>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, bool>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })
        .map_err(db_error)?;
    for row in rows {
        let (
            run,
            format,
            path,
            start,
            start_column,
            end,
            end_column,
            count,
            test,
            complete,
            run_id,
        ) = row.map_err(db_error)?;
        if scope.is_some_and(|scope| !scope.relevant(&path, start, end)) {
            continue;
        }
        relevant_runs.insert(run_id);
        let count =
            u64::try_from(count).map_err(|_| "database coverage count is invalid".to_owned())?;
        let mut block = String::new();
        push_execution_claim(
            &mut block,
            &run,
            &format,
            &path,
            i64::from(start),
            i64::from(end),
            count,
            test.as_deref(),
            complete,
        )?;
        block.push_str(if count == 0 {
            "not-observed run="
        } else {
            "observed run="
        });
        block.push_str(&format!("{run:?}"));
        if let Some(test) = &test {
            block.push_str(&format!(" test={test:?}"));
        }
        block.push_str(&format!(
            " path={path:?} lines={} count={count}\n",
            line_range(start, end),
        ));
        observations.push(CoverageOutput {
            run,
            path,
            start: i64::from(start),
            start_column,
            end: i64::from(end),
            end_column,
            named: test.is_some(),
            kind: String::new(),
            block,
        });
    }

    let mut branches = connection
        .prepare(
            "SELECT run.run_label, run.format, file.path,
                    branch.start_line, branch.start_column,
                    branch.end_line, branch.end_column, branch.target_line,
                    branch.kind, branch.execution_count,
                    CASE WHEN branch.test_id IS NOT NULL THEN run.test_name END,
                    NOT EXISTS(
                        SELECT 1 FROM graph_gaps gap
                         WHERE gap.run_id=run.id AND gap.category='coverage' AND (
                            (run.format='llvm'
                             AND gap.reason IN ('missing-test-context','ambiguous-test-context')
                             AND gap.target_hint=run.test_name)
                            OR (gap.reason IN ('coverage-unmapped-file','coverage-unmapped-region')
                                AND gap.path=file.path AND (
                                    (branch.kind!='arc'
                                     AND gap.line_start<=branch.end_line
                                     AND gap.line_end>=branch.start_line)
                                    OR (branch.kind='arc' AND (
                                        (branch.start_line>0
                                         AND gap.line_start<=branch.start_line
                                         AND gap.line_end>=branch.start_line)
                                        OR (branch.target_line>0
                                            AND gap.line_start<=branch.target_line
                                            AND gap.line_end>=branch.target_line)
                                    ))
                                ))
                         )
                    ), run.id
               FROM coverage_branches branch
               JOIN coverage_runs run ON run.id=branch.run_id
               JOIN files file ON file.id=branch.file_id
              ORDER BY (branch.test_id IS NULL), run.run_label, file.path,
                       branch.start_line, branch.start_column, branch.end_line,
                       branch.end_column, branch.target_line, branch.kind, branch.id",
        )
        .map_err(db_error)?;
    let rows = branches
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, u32>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, bool>(11)?,
                row.get::<_, i64>(12)?,
            ))
        })
        .map_err(db_error)?;
    for row in rows {
        let (
            run,
            format,
            path,
            start,
            start_column,
            end,
            end_column,
            target,
            kind,
            count,
            test,
            complete,
            run_id,
        ) = row.map_err(db_error)?;
        if scope.is_some_and(|scope| !scope.relevant_branch(&path, start, end, target, &kind)) {
            continue;
        }
        relevant_runs.insert(run_id);
        let count =
            u64::try_from(count).map_err(|_| "database coverage count is invalid".to_owned())?;
        let mut block = String::new();
        push_execution_claim(
            &mut block,
            &run,
            &format,
            &path,
            start,
            end,
            count,
            test.as_deref(),
            complete,
        )?;
        block.push_str(if count == 0 {
            "not-observed-branch run="
        } else {
            "observed-branch run="
        });
        block.push_str(&format!("{run:?}"));
        if let Some(test) = &test {
            block.push_str(&format!(" test={test:?}"));
        }
        let arm = match kind.as_str() {
            "true-outcome" => "true".to_owned(),
            "false-outcome" => "false".to_owned(),
            "arc" => format!("target:{}", target.unwrap_or_default()),
            _ => return Err("database coverage branch kind is invalid".into()),
        };
        block.push_str(&format!(
            " path={path:?} line={start} arm={arm} count={count}\n"
        ));
        observations.push(CoverageOutput {
            run,
            path,
            start,
            start_column,
            end,
            end_column,
            named: test.is_some(),
            kind,
            block,
        });
    }

    observations.sort_unstable_by(|left, right| {
        (
            !left.named,
            &left.run,
            &left.path,
            left.start,
            left.start_column,
            left.end,
            left.end_column,
            &left.kind,
        )
            .cmp(&(
                !right.named,
                &right.run,
                &right.path,
                right.start,
                right.start_column,
                right.end,
                right.end_column,
                &right.kind,
            ))
    });
    for observation in observations {
        text.push_str(&observation.block);
    }

    let mut gaps = connection
        .prepare(
            "SELECT run.run_label, run.format, gap.reason, gap.path,
                    gap.line_start, gap.line_end, gap.occurrences,
                    gap.target_hint, run.id
               FROM graph_gaps gap JOIN coverage_runs run ON run.id=gap.run_id
              WHERE gap.category='coverage'
              ORDER BY run.run_label, gap.path, gap.line_start, gap.line_end,
                       gap.reason, gap.target_hint, gap.id",
        )
        .map_err(db_error)?;
    let rows = gaps
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<u32>>(4)?,
                row.get::<_, Option<u32>>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })
        .map_err(db_error)?;
    for row in rows {
        let (run, format, reason, path, start, end, occurrences, target, run_id) =
            row.map_err(db_error)?;
        if let Some(scope) = scope {
            let relevant = match path.as_deref() {
                Some(path) => scope.relevant(path, start.unwrap_or(1), end.unwrap_or(u32::MAX)),
                None => {
                    reason == "coverage-unmapped-file"
                        || matches!(
                            reason.as_str(),
                            "missing-test-context" | "ambiguous-test-context"
                        ) && relevant_runs.contains(&run_id)
                }
            };
            if !relevant {
                continue;
            }
        }
        text.push_str("claim kind=changed-execution");
        if let Some(path) = &path {
            text.push_str(&format!(" path={path:?}"));
        }
        if let Some(start) = start {
            text.push_str(&format!(
                " lines={}",
                line_range(start, end.unwrap_or(start))
            ));
        }
        text.push_str(&format!(
            " status=partial result=unknown basis={} run={run:?}",
            coverage_basis(&format)?
        ));
        if matches!(
            reason.as_str(),
            "missing-test-context" | "ambiguous-test-context"
        ) && let Some(test) = &target
        {
            text.push_str(&format!(" test={test:?}"));
        }
        text.push('\n');
        text.push_str(&format!(
            "gap category=coverage reason={reason} run={run:?}"
        ));
        if let Some(path) = path {
            text.push_str(&format!(" path={path:?}"));
        }
        if let Some(start) = start {
            text.push_str(&format!(
                " line={}",
                line_range(start, end.unwrap_or(start))
            ));
        }
        if let Some(target) = target {
            text.push_str(&format!(" target={target:?}"));
        }
        text.push_str(&format!(" occurrences={occurrences}\n"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_execution_claim(
    text: &mut String,
    run: &str,
    format: &str,
    path: &str,
    start: i64,
    end: i64,
    count: u64,
    test: Option<&str>,
    complete: bool,
) -> Result<()> {
    let basis = coverage_basis(format)?;
    text.push_str(&format!(
        "claim kind=changed-execution path={path:?} lines={} status={} result={} basis={} run={run:?}",
        line_range_i64(start, end),
        if complete { "complete" } else { "partial" },
        if complete {
            if count == 0 { "not-observed" } else { "observed" }
        } else {
            "unknown"
        },
        basis,
    ));
    if let Some(test) = test {
        text.push_str(&format!(" test={test:?}"));
    }
    text.push('\n');
    Ok(())
}

fn coverage_basis(format: &str) -> Result<&'static str> {
    match format {
        "llvm" => Ok("llvm-coverage-json"),
        "coverage_py" => Ok("coverage-py-json"),
        _ => Err("database coverage format is invalid".into()),
    }
}

fn line_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn line_range_i64(start: i64, end: i64) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn assurance_preamble(
    accounting: &StaticAccounting,
    content_complete: bool,
    mapping_complete: bool,
    traversal_complete: bool,
    evidence: &EvidenceReview,
) -> String {
    let status = |complete| if complete { "complete" } else { "partial" };
    let static_status = accounting
        .overall(content_complete, mapping_complete, traversal_complete)
        .as_str();
    let mut by_reason = String::new();
    for (index, (reason, occurrences)) in accounting.gaps_by_reason.iter().enumerate() {
        if index > 0 {
            by_reason.push(',');
        }
        by_reason.push_str(reason.db());
        by_reason.push(':');
        by_reason.push_str(&occurrences.to_string());
    }
    if by_reason.is_empty() {
        by_reason.push_str("none");
    }
    let static_test_paths = if evidence.execution_status == "not-applicable" {
        static_test_paths_claim(static_status)
    } else {
        String::new()
    };
    format!(
        "languages=rust,python,javascript,typescript\n\
completeness content_capture={} source_capture={} syntax_parse={} site_classification=complete static_model={} evidence_capture={} provenance_model={} execution_mapping={} traversal={}\n\
gaps total={} relevant={} by_reason={}\n\
references missing={} ambiguous={}\n\
{}\
claim kind=affected-callers status={static_status} basis=resolved-static-call-graph\n\
claim kind=affected-flows status={static_status} basis=resolved-static-call-graph\n\
{static_test_paths}",
        status(content_complete),
        status(accounting.source_complete),
        status(accounting.syntax_complete),
        status(accounting.static_model_complete && mapping_complete),
        evidence.capture_status,
        evidence.provenance_status,
        evidence.execution_status,
        status(traversal_complete),
        accounting.total_gaps,
        accounting.relevant_gaps,
        by_reason,
        accounting.missing,
        accounting.ambiguous,
        accounting.gap_records,
    )
}

fn ordered_evidence_text(
    mut evidence: EvidenceReview,
    static_status: CompletenessStatus,
) -> String {
    if evidence.execution_status != "not-applicable" {
        evidence
            .text
            .push_str(&static_test_paths_claim(static_status.as_str()));
    }
    evidence.text
}

fn static_test_paths_claim(status: &str) -> String {
    format!("claim kind=static-test-paths status={status} basis=resolved-static-call-graph\n")
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

fn flow_path(flow: &AffectedFlow, target: i64, depth: u32) -> Result<Vec<i64>> {
    let mut path = vec![target];
    while path.last().copied() != Some(flow.entry.id) {
        let current = *path.last().expect("path starts at its target");
        let parent = flow
            .parents
            .get(&current)
            .copied()
            .ok_or_else(|| "affected flow path is incomplete".to_owned())?;
        if path.contains(&parent) || path.len() > FLOW_DEPTH as usize {
            return Err("affected flow path is cyclic".into());
        }
        path.push(parent);
    }
    path.reverse();
    let keep = depth as usize + 1;
    if path.len() > keep {
        path.drain(..path.len() - keep);
    }
    Ok(path)
}

pub(crate) fn no_change_dot(snapshot_id: &str, reason: &str) -> String {
    format!(
        "digraph graphr_changes {{\n  graph [rankdir=LR, label=\"snapshot={} no_changes_reason={}\"];\n}}\n",
        dot_escape(&shorten(snapshot_id, DOT_LABEL_PART_LIMIT)),
        dot_escape(&shorten(reason, DOT_LABEL_PART_LIMIT)),
    )
}

fn change_dot(
    snapshot_id: &str,
    roots: &[RowNode],
    analysis: &ChangeAnalysis,
    calls: &ChangeCalls,
    limits: (u32, u32),
    dependency_mode: DependencyMode,
    accounting: DotAccounting,
) -> Result<String> {
    let (_, max_nodes) = limits;
    let max_nodes = max_nodes as usize;
    let direct_ids = roots
        .iter()
        .filter(|root| root.kind != "file" && analysis.risks.contains_key(&root.id))
        .map(|root| root.id)
        .collect::<Vec<_>>();
    let mut catalog = HashMap::new();
    for root in roots {
        catalog.insert(root.id, root.clone());
    }
    for flow in &analysis.flows {
        for node in std::iter::once(&flow.entry).chain(&flow.nodes) {
            catalog.entry(node.id).or_insert_with(|| RowNode {
                id: node.id,
                kind: node.kind.clone(),
                name: node.name.clone(),
                path: node.path.clone(),
                line: node.line,
            });
        }
    }
    for node in calls.nodes.values() {
        catalog.entry(node.id).or_insert_with(|| node.clone());
    }

    let mut paths = Vec::new();
    for flow in &analysis.flows {
        let mut targets = flow.changed.clone();
        targets.sort_unstable();
        for target in targets {
            paths.push(flow_path(flow, target, limits.0)?);
        }
    }
    let paths_discovered = paths.len();
    let mut direct_count = direct_ids.len().min(max_nodes);
    let mut selected_ids = direct_ids[..direct_count].to_vec();
    let mut selected_set = selected_ids.iter().copied().collect::<HashSet<_>>();
    let mut selected_paths = Vec::new();
    for path in paths {
        if path.iter().any(|id| !catalog.contains_key(id)) {
            return Err("affected flow node is missing".into());
        }
        let additions = path.iter().filter(|id| !selected_set.contains(id)).count();
        if selected_ids.len().saturating_add(additions) <= max_nodes {
            for id in &path {
                if selected_set.insert(*id) {
                    selected_ids.push(*id);
                }
            }
            selected_paths.push(path);
        }
    }

    let mut budget_pruned = false;
    let mut include_call_context = true;
    loop {
        let (dot, _) = render_change_dot(
            snapshot_id,
            &catalog,
            &direct_ids[..direct_count],
            &selected_paths,
            calls,
            include_call_context,
            dependency_mode,
            accounting,
            paths_discovered,
            direct_count == direct_ids.len()
                && selected_paths.len() == paths_discovered
                && !budget_pruned,
            max_nodes,
            analysis,
        );
        if dot.len() <= DOT_BUDGET {
            return Ok(dot);
        }
        budget_pruned = true;
        if selected_paths.pop().is_some() {
            continue;
        }
        if include_call_context && !calls.edges.is_empty() {
            include_call_context = false;
            continue;
        }
        if direct_count == 0 {
            return Ok(dot);
        }
        direct_count -= 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn render_change_dot(
    snapshot_id: &str,
    catalog: &HashMap<i64, RowNode>,
    direct_ids: &[i64],
    paths: &[Vec<i64>],
    calls: &ChangeCalls,
    include_call_context: bool,
    dependency_mode: DependencyMode,
    accounting: DotAccounting,
    paths_discovered: usize,
    mut render_complete: bool,
    max_nodes: usize,
    analysis: &ChangeAnalysis,
) -> (String, bool) {
    let changed_ids = direct_ids.iter().copied().collect::<HashSet<_>>();
    let mut selected = direct_ids.to_vec();
    let mut selected_set = changed_ids.clone();
    let mut impact_ids = HashSet::new();
    let mut impact_order = Vec::new();
    let mut edges = BTreeMap::<(i64, i64), bool>::new();
    for path in paths {
        for id in path {
            if selected_set.insert(*id) {
                selected.push(*id);
            }
        }
        if let Some(target) = path.last()
            && !analysis.risks.contains_key(target)
            && impact_ids.insert(*target)
        {
            impact_order.push(*target);
        }
        for edge in path.windows(2) {
            edges.insert((edge[0], edge[1]), false);
        }
    }

    let mut pending = direct_ids
        .iter()
        .copied()
        .chain(impact_order)
        .collect::<VecDeque<_>>();
    let mut searched = HashSet::new();
    let mut calls_complete = include_call_context || calls.edges.is_empty();
    if include_call_context {
        while let Some(callee) = pending.pop_front() {
            if !searched.insert(callee) {
                continue;
            }
            for &(caller, _, is_test_call) in calls
                .edges
                .iter()
                .filter(|(_, target, _)| *target == callee)
            {
                if selected_set.insert(caller) {
                    if selected.len() == max_nodes {
                        selected_set.remove(&caller);
                        calls_complete = false;
                        continue;
                    }
                    selected.push(caller);
                }
                if selected_set.contains(&caller) {
                    pending.push_back(caller);
                }
                edges
                    .entry((caller, callee))
                    .and_modify(|dashed| *dashed |= is_test_call)
                    .or_insert(is_test_call);
            }
        }
        for &(caller, callee, is_test_call) in &calls.edges {
            if selected_set.contains(&caller) && selected_set.contains(&callee) {
                edges
                    .entry((caller, callee))
                    .and_modify(|dashed| *dashed |= is_test_call)
                    .or_insert(is_test_call);
            }
        }
    }
    render_complete &= calls_complete;

    let flow_discovery = if analysis.flow_omitted
        || accounting.analysis_roots_omitted > 0
        || accounting.deleted_paths_unanalyzed > 0
    {
        "partial"
    } else {
        "complete"
    };
    let mut output = format!(
        "digraph graphr_changes {{\n  graph [rankdir=LR, label=\"snapshot={} changed_emitted={} changed_total={} paths_emitted={} paths_discovered={} flow_discovery={} render_complete={} analysis_roots_omitted={} deleted_paths_unanalyzed={} unmapped_ranges={} file_mapped_ranges={} traversal_complete={}\"];\n",
        dot_escape(&shorten(snapshot_id, DOT_LABEL_PART_LIMIT)),
        direct_ids.len(),
        accounting.changed_total,
        paths.len(),
        paths_discovered,
        flow_discovery,
        render_complete,
        accounting.analysis_roots_omitted,
        accounting.deleted_paths_unanalyzed,
        accounting.unmapped_ranges,
        accounting.file_mapped_ranges,
        accounting.traversal_complete,
    );
    for id in selected {
        let node = &catalog[&id];
        let test_shape = if node.kind == "test" {
            "shape=ellipse, "
        } else {
            ""
        };
        let attributes = if changed_ids.contains(&node.id) {
            format!(
                "{}fillcolor=\"#fed7aa\", color=\"#c2410c\", penwidth=2, label=\"{}\\n{}:{}\\nchanged risk={}\"",
                test_shape,
                dot_escape(&shorten(&node.name, DOT_LABEL_PART_LIMIT)),
                dot_escape(&shorten(&node.path, DOT_LABEL_PART_LIMIT)),
                node.line,
                score_text(analysis.risks.get(&node.id).map_or(0, |risk| risk.score)),
            )
        } else if impact_ids.contains(&node.id) {
            format!(
                "{}fillcolor=\"#fef3c7\", color=\"#a16207\", label=\"{}\\n{}:{}\\naffected\"",
                test_shape,
                dot_escape(&shorten(&node.name, DOT_LABEL_PART_LIMIT)),
                dot_escape(&shorten(&node.path, DOT_LABEL_PART_LIMIT)),
                node.line,
            )
        } else if dependency_mode == DependencyMode::Boundary
            && dependency_package(&node.path).is_some()
        {
            format!(
                "{}fillcolor=\"#e5e7eb\", color=\"#4b5563\", label=\"{}\\n{}:{}\"",
                test_shape,
                dot_escape(&shorten(&node.name, DOT_LABEL_PART_LIMIT)),
                dot_escape(&shorten(&node.path, DOT_LABEL_PART_LIMIT)),
                node.line,
            )
        } else if node.kind == "test" {
            format!(
                "shape=ellipse, fillcolor=\"#dbeafe\", color=\"#2563eb\", label=\"{}\\n{}:{}\"",
                dot_escape(&shorten(&node.name, DOT_LABEL_PART_LIMIT)),
                dot_escape(&shorten(&node.path, DOT_LABEL_PART_LIMIT)),
                node.line,
            )
        } else {
            format!(
                "label=\"{}\\n{}:{}\"",
                dot_escape(&shorten(&node.name, DOT_LABEL_PART_LIMIT)),
                dot_escape(&shorten(&node.path, DOT_LABEL_PART_LIMIT)),
                node.line,
            )
        };
        output.push_str(&format!("  n{} [style=filled, {}];\n", node.id, attributes));
    }
    for ((caller, callee), dashed) in edges {
        if dashed {
            output.push_str(&format!("  n{caller} -> n{callee} [style=dashed];\n"));
        } else {
            output.push_str(&format!("  n{caller} -> n{callee};\n"));
        }
    }
    output.push_str("}\n");
    // ponytail: re-rendering is bounded to 50 nodes; stream with reserved bytes only if that cap grows.
    (output, calls_complete)
}

fn shorten(value: &str, limit: usize) -> Cow<'_, str> {
    if value.len() <= limit {
        return Cow::Borrowed(value);
    }
    let mut end = limit.saturating_sub('…'.len_utf8()).min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}…", &value[..end]))
}

fn dot_escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' | '\r' => output.push_str("\\n"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => output.push('�'),
            value => output.push(value),
        }
    }
    output
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
            "SELECT id, path, language, git_oid, content_hash, parse_context, byte_size,
                    observed_relation_sites
               FROM files",
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
                row.get::<_, i64>(7)?,
            ))
        })
        .map_err(db_error)?;
    let mut files = HashMap::new();
    for row in rows {
        let (id, path, language, git_oid, hash, parse_context, byte_size, observed_relation_sites) =
            row.map_err(db_error)?;
        if id <= 0
            || byte_size < 0
            || observed_relation_sites < 0
            || !git_oid.as_deref().is_none_or(valid_oid)
        {
            return Err("database file metadata is invalid".into());
        }
        let language = Language::parse(&language)
            .ok_or_else(|| "database file language is invalid".to_owned())?;
        let content_hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| "database content hash is invalid".to_owned())?;
        let byte_size =
            u64::try_from(byte_size).map_err(|_| "database file size is invalid".to_owned())?;
        let observed_relation_sites = u32::try_from(observed_relation_sites)
            .map_err(|_| "database relation-site count is invalid".to_owned())?;
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
                    observed_relation_sites,
                },
            )
            .is_some()
        {
            return Err("database contains duplicate file paths".into());
        }
    }
    Ok(files)
}

fn load_source_global_gaps(
    connection: &Connection,
    cancelled: &AtomicBool,
) -> Result<Vec<GapInput>> {
    let mut statement = connection
        .prepare(
            "SELECT path, line_start, line_end, category, reason, target_hint,
                    occurrences, relation_site
               FROM graph_gaps
              WHERE file_id IS NULL AND source_id IS NULL AND run_id IS NULL
                AND category NOT IN ('generated','coverage')
              ORDER BY path, line_start, line_end, category, reason, target_hint, id",
        )
        .map_err(db_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<u32>>(1)?,
                row.get::<_, Option<u32>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, bool>(7)?,
            ))
        })
        .map_err(db_error)?;
    let mut gaps = Vec::new();
    for row in rows {
        check_cancelled(cancelled)?;
        let (path, line_start, line_end, category, reason, target_hint, occurrences, relation_site) =
            row.map_err(db_error)?;
        gaps.push(GapInput {
            file_key: None,
            source_key: None,
            run_key: None,
            path,
            line_start,
            line_end,
            category: GapCategory::parse(&category)
                .ok_or_else(|| "database gap category is invalid".to_owned())?,
            reason: GapReason::parse(&reason)
                .ok_or_else(|| "database gap reason is invalid".to_owned())?,
            target_hint,
            occurrences,
            relation_site,
        });
    }
    Ok(gaps)
}

fn apply_incremental(
    tx: &Transaction<'_>,
    graph: &Graph,
    existing: &HashMap<String, StoredFile>,
    cancelled: &AtomicBool,
) -> Result<usize> {
    if !graph.edges.is_empty()
        || graph.refs.iter().any(|reference| {
            reference.resolved_target_key.is_some()
                || reference.resolution != ResolutionState::Pending
        })
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
    let stored_global_gaps = tx
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM graph_gaps
                  WHERE file_id IS NULL AND source_id IS NULL AND run_id IS NULL
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(db_error)?;
    let current_global_gaps = graph.gaps.iter().any(global_gap);
    let replace_global_gaps = stored_global_gaps || current_global_gaps;
    let changed = removed.len()
        + graph
            .files
            .iter()
            .filter(|file| file.replace && !existing.contains_key(&file.path))
            .count()
        + usize::from(replace_global_gaps);
    if changed == 0
        && (!graph.nodes.is_empty()
            || !graph.refs.is_empty()
            || !graph.trait_implementations.is_empty()
            || !graph.modeled_sites.is_empty()
            || graph.gaps.iter().any(|gap| !global_gap(gap)))
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
            || old.observed_relation_sites != file.observed_relation_sites
    });
    if metadata_changed {
        let mut update = tx
            .prepare(
                "UPDATE files
                    SET git_oid=?1, content_hash=?2, parse_context=?3, byte_size=?4,
                        observed_relation_sites=?5
                  WHERE id=?6",
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
                || old.observed_relation_sites != file.observed_relation_sites
            {
                update
                    .execute(params![
                        file.git_oid,
                        file.content_hash.as_slice(),
                        file.parse_context,
                        i64::try_from(file.byte_size)
                            .map_err(|_| "file size exceeds SQLite range".to_owned())?,
                        file.observed_relation_sites,
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
                "SELECT nk.key, n.kind FROM node_keys nk
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
                .query_map([file_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(db_error)?
            {
                let (key, kind) = row.map_err(db_error)?;
                if kind == "type" {
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
            if node.kind == NodeKind::Type {
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
        // ponytail: rebuilding derived reference edges keeps candidate removal atomic;
        // narrow this to affected refs only if incremental profiling requires it.
        tx.execute("DELETE FROM edges", []).map_err(db_error)?;
        tx.execute(
            "UPDATE refs SET resolved_target_id=NULL, resolution_state='pending'",
            [],
        )
        .map_err(db_error)?;
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

    if replace_global_gaps {
        tx.execute(
            "DELETE FROM graph_gaps
              WHERE file_id IS NULL AND source_id IS NULL AND run_id IS NULL",
            [],
        )
        .map_err(db_error)?;
    }

    let (new_refs, new_implementations) = insert_graph(tx, graph, cancelled, true)?;
    affected_refs.extend(new_refs);
    affected_refs.extend(
        tx.prepare("SELECT id FROM refs ORDER BY id")
            .map_err(db_error)?
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?,
    );
    affected_implementations.extend(new_implementations);
    resolve_references(tx, affected_refs, cancelled)?;
    refresh_script_export_methods(tx, cancelled)?;
    resolve_trait_implementations(tx, affected_implementations, cancelled)?;
    reparent_methods(tx, affected_owners, cancelled)?;
    Ok(changed)
}

fn global_gap(gap: &GapInput) -> bool {
    gap.file_key.is_none() && gap.source_key.is_none() && gap.run_key.is_none()
}

struct ProvenanceResolution {
    generator_file_id: Option<i64>,
    generator_node_id: Option<i64>,
    modeled_site_id: Option<i64>,
    mapping_state: &'static str,
}

fn provenance_resolution(
    connection: &Connection,
    generator_path: &str,
    generator_lines: EvidenceLineSpan,
    output_basename: &str,
    basename_contended: bool,
) -> Result<ProvenanceResolution> {
    let generators = connection
        .prepare(
            "SELECT f.id, n.id FROM files f JOIN nodes n ON n.file_id=f.id
              WHERE f.path=?1 AND f.language IN ('rust','python')
                AND n.kind IN ('function','test')
                AND n.line_start<=?2 AND n.line_end>=?3
              ORDER BY n.id LIMIT 2",
        )
        .map_err(db_error)?
        .query_map(
            params![generator_path, generator_lines.start, generator_lines.end],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    let sites = if basename_contended {
        Vec::new()
    } else {
        connection
            .prepare(
                "SELECT m.id FROM modeled_sites m
                  WHERE m.kind='generated-inclusion' AND m.target_hint=?1
                    AND m.parse_context IS NOT NULL
                  ORDER BY m.id LIMIT 2",
            )
            .map_err(db_error)?
            .query_map([output_basename], |row| row.get::<_, i64>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?
    };
    let ambiguous = basename_contended || generators.len() > 1 || sites.len() > 1;
    let state = if ambiguous {
        "ambiguous"
    } else if generators.len() == 1 && sites.len() == 1 {
        "linked"
    } else {
        "unobserved"
    };
    let (generator_file_id, generator_node_id) = generators
        .first()
        .filter(|_| generators.len() == 1)
        .copied()
        .map_or((None, None), |(file, node)| (Some(file), Some(node)));
    let modeled_site_id = sites.first().filter(|_| sites.len() == 1).copied();
    Ok(ProvenanceResolution {
        generator_file_id,
        generator_node_id,
        modeled_site_id,
        mapping_state: state,
    })
}

fn insert_evidence(
    tx: &Transaction<'_>,
    evidence: &EvidenceInput,
    generated_files: usize,
    cancelled: &AtomicBool,
) -> Result<EvidenceStats> {
    let mut artifacts = HashMap::<String, (i64, ArtifactRole, [u8; 32])>::new();
    let mut artifact_paths = HashMap::<String, String>::new();
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO imported_artifacts(key, role, path, content_hash, byte_size)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(db_error)?;
        for artifact in &evidence.artifacts {
            check_cancelled(cancelled)?;
            if !crate::evidence::evidence_path_is_safe(&artifact.path) {
                return Err("evidence artifact path is unsafe".into());
            }
            let byte_size = i64::try_from(artifact.byte_size)
                .map_err(|_| "artifact size exceeds SQLite range".to_owned())?;
            insert
                .execute(params![
                    artifact.key,
                    artifact.role.db(),
                    artifact.path,
                    artifact.content_hash.as_slice(),
                    byte_size
                ])
                .map_err(db_error)?;
            if artifacts
                .insert(
                    artifact.key.clone(),
                    (tx.last_insert_rowid(), artifact.role, artifact.content_hash),
                )
                .is_some()
            {
                return Err("duplicate evidence artifact key".into());
            }
            artifact_paths.insert(artifact.key.clone(), artifact.path.clone());
        }
    }

    let mut output_paths_by_basename = BTreeMap::<String, BTreeSet<String>>::new();
    for provenance in &evidence.provenance {
        let output_path = artifact_paths
            .get(&provenance.output_key)
            .ok_or_else(|| "provenance references an unknown output artifact".to_owned())?;
        let basename = Path::new(output_path)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "provenance output basename is invalid".to_owned())?;
        output_paths_by_basename
            .entry(basename.to_owned())
            .or_default()
            .insert(output_path.clone());
    }
    let mut inserted_links = 0_usize;
    let mut declarations = HashSet::new();
    for provenance in &evidence.provenance {
        check_cancelled(cancelled)?;
        if !crate::evidence::evidence_path_is_safe(&provenance.generator_path) {
            return Err("provenance generator path is unsafe".into());
        }
        validate_evidence_span(provenance.input_lines)?;
        validate_evidence_span(provenance.generator_lines)?;
        validate_evidence_span(provenance.output_lines)?;
        let (input_id, input_role, _) = artifacts
            .get(&provenance.input_key)
            .copied()
            .ok_or_else(|| "provenance references an unknown input artifact".to_owned())?;
        let (output_id, output_role, _) = artifacts
            .get(&provenance.output_key)
            .copied()
            .ok_or_else(|| "provenance references an unknown output artifact".to_owned())?;
        if input_role != ArtifactRole::Input || output_role != ArtifactRole::GeneratedRust {
            return Err("provenance artifact role is invalid".into());
        }
        let declaration = (
            input_id,
            provenance.input_lines.start,
            provenance.input_lines.end,
            provenance.generator_path.clone(),
            provenance.generator_lines.start,
            provenance.generator_lines.end,
            output_id,
            provenance.output_lines.start,
            provenance.output_lines.end,
        );
        if !declarations.insert(declaration) {
            return Err("duplicate provenance declaration".into());
        }
        let output_path = &artifact_paths[&provenance.output_key];
        let basename = Path::new(output_path)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "provenance output basename is invalid".to_owned())?;
        let basename_contended = output_paths_by_basename
            .get(basename)
            .is_some_and(|paths| paths.len() > 1);
        let resolution = provenance_resolution(
            tx,
            &provenance.generator_path,
            provenance.generator_lines,
            basename,
            basename_contended,
        )?;
        if let Some(site_id) = resolution.modeled_site_id {
            tx.execute(
                "DELETE FROM graph_gaps
                  WHERE reason='generated-output-unobserved'
                    AND file_id=(SELECT file_id FROM modeled_sites WHERE id=?1)
                    AND line_start=(SELECT line_start FROM modeled_sites WHERE id=?1)
                    AND line_end=(SELECT line_end FROM modeled_sites WHERE id=?1)
                    AND ifnull(target_hint,'')=ifnull(
                        (SELECT target_hint FROM modeled_sites WHERE id=?1), '')",
                [site_id],
            )
            .map_err(db_error)?;
        }
        tx.execute(
            "INSERT INTO provenance_links(
                input_artifact_id, input_line_start, input_line_end,
                generator_path, generator_file_id, generator_node_id,
                generator_line_start, generator_line_end,
                output_artifact_id, output_line_start, output_line_end,
                modeled_site_id, mapping_state
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                input_id,
                provenance.input_lines.start,
                provenance.input_lines.end,
                provenance.generator_path,
                resolution.generator_file_id,
                resolution.generator_node_id,
                provenance.generator_lines.start,
                provenance.generator_lines.end,
                output_id,
                provenance.output_lines.start,
                provenance.output_lines.end,
                resolution.modeled_site_id,
                resolution.mapping_state
            ],
        )
        .map_err(db_error)?;
        inserted_links += usize::from(resolution.mapping_state == "linked");
    }

    let mut runs = HashMap::<String, (i64, CoverageFormat, Option<i64>)>::new();
    for run in &evidence.runs {
        check_cancelled(cancelled)?;
        let (report_id, role, report_digest) = artifacts
            .get(&run.report_key)
            .copied()
            .ok_or_else(|| "coverage run references an unknown report artifact".to_owned())?;
        if role != ArtifactRole::CoverageReport {
            return Err("coverage report artifact role is invalid".into());
        }
        validate_coverage_label(&run.run_label)?;
        if let Some(test_name) = &run.test_name {
            validate_coverage_label(test_name)?;
        }
        let (test_id, mapping) = run
            .test_name
            .as_deref()
            .map(|name| coverage_test_mapping(tx, name))
            .transpose()?
            .unwrap_or((None, None));
        tx.execute(
            "INSERT INTO coverage_runs(
                key, report_artifact_id, report_digest, format, run_label, test_name, test_id
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.key,
                report_id,
                report_digest.as_slice(),
                run.format.db(),
                run.run_label,
                run.test_name,
                test_id,
            ],
        )
        .map_err(db_error)?;
        let run_id = tx.last_insert_rowid();
        if runs
            .insert(run.key.clone(), (run_id, run.format, test_id))
            .is_some()
        {
            return Err("duplicate coverage run key".into());
        }
        if let Some(reason) = mapping {
            insert_coverage_gap(
                tx,
                run_id,
                None,
                None,
                None,
                reason,
                run.test_name.as_deref(),
                1,
            )?;
        }
    }
    for region in &evidence.regions {
        check_cancelled(cancelled)?;
        validate_coverage_range(
            region.start_line,
            region.start_column,
            region.end_line,
            region.end_column,
        )?;
        let (run_id, format, run_test_id) = runs
            .get(&region.run_key)
            .copied()
            .ok_or_else(|| "coverage region run is unknown".to_owned())?;
        let file_id = evidence_file_id(tx, region.path.as_deref())?;
        if let Some(context) = &region.context {
            validate_coverage_label(context)?;
        }
        let (context_test_id, mapping) = region
            .context
            .as_deref()
            .map(|name| coverage_test_mapping(tx, name))
            .transpose()?
            .unwrap_or((None, None));
        let test_id = if region.context.is_some() {
            context_test_id
        } else if format == CoverageFormat::Llvm {
            run_test_id
        } else {
            None
        };
        if let Some(reason) = mapping {
            insert_coverage_gap(
                tx,
                run_id,
                region.path.as_deref(),
                Some(region.start_line),
                Some(region.end_line),
                reason,
                region.context.as_deref(),
                1,
            )?;
        }
        record_coverage_mapping_gap(
            tx,
            run_id,
            file_id,
            region.path.as_deref(),
            region.start_line,
            region.end_line,
        )?;
        tx.execute(
            "INSERT INTO coverage_regions(
                run_id, file_id, path, test_id, start_line, start_column, end_line, end_column,
                execution_count, context
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                run_id,
                file_id,
                region.path,
                test_id,
                region.start_line,
                region.start_column,
                region.end_line,
                region.end_column,
                i64::try_from(region.execution_count)
                    .map_err(|_| "coverage count exceeds SQLite range".to_owned())?,
                region.context
            ],
        )
        .map_err(db_error)?;
    }
    for branch in &evidence.branches {
        check_cancelled(cancelled)?;
        let (mapping_start, mapping_end) = validate_coverage_branch(
            branch.start_line,
            branch.start_column,
            branch.end_line,
            branch.end_column,
            branch.target_line,
            branch.kind,
        )?;
        if branch.kind == CoverageBranchKind::Arc && branch.target_line.is_none()
            || branch.kind != CoverageBranchKind::Arc && branch.target_line.is_some()
        {
            return Err("coverage branch target is invalid".into());
        }
        let (run_id, format, run_test_id) = runs
            .get(&branch.run_key)
            .copied()
            .ok_or_else(|| "coverage branch run is unknown".to_owned())?;
        let file_id = evidence_file_id(tx, branch.path.as_deref())?;
        let test_id = (format == CoverageFormat::Llvm)
            .then_some(run_test_id)
            .flatten();
        record_coverage_mapping_gap(
            tx,
            run_id,
            file_id,
            branch.path.as_deref(),
            mapping_start,
            mapping_end,
        )?;
        tx.execute(
            "INSERT INTO coverage_branches(
                run_id, file_id, path, test_id, start_line, start_column, end_line, end_column,
                target_line, kind, execution_count
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run_id,
                file_id,
                branch.path,
                test_id,
                branch.start_line,
                branch.start_column,
                branch.end_line,
                branch.end_column,
                branch.target_line,
                branch.kind.db(),
                i64::try_from(branch.execution_count)
                    .map_err(|_| "coverage count exceeds SQLite range".to_owned())?
            ],
        )
        .map_err(db_error)?;
    }
    for gap in &evidence.gaps {
        check_cancelled(cancelled)?;
        if gap.file_key.is_some() || gap.source_key.is_some() {
            return Err("evidence gap ownership is invalid".into());
        }
        if gap.category != GapCategory::Coverage {
            return Err("evidence gap category is invalid".into());
        }
        let run_id = gap
            .run_key
            .as_deref()
            .map(|key| {
                runs.get(key)
                    .map(|(id, _, _)| *id)
                    .ok_or_else(|| "evidence gap run is unknown".to_owned())
            })
            .transpose()?;
        if run_id.is_some() != (gap.category == GapCategory::Coverage) {
            return Err("evidence gap ownership is invalid".into());
        }
        if gap.category == GapCategory::Coverage
            && !matches!(
                gap.reason,
                GapReason::CoverageUnmappedFile
                    | GapReason::CoverageUnmappedRegion
                    | GapReason::MissingTestContext
                    | GapReason::AmbiguousTestContext
            )
        {
            return Err("coverage gap reason is invalid".into());
        }
        tx.execute(
            "INSERT INTO graph_gaps(
                file_id, source_id, run_id, path, line_start, line_end, category, reason,
                target_hint, occurrences, relation_site
             ) VALUES(NULL, NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT DO UPDATE
                 SET occurrences=graph_gaps.occurrences+excluded.occurrences",
            params![
                run_id,
                gap.path,
                gap.line_start,
                gap.line_end,
                gap.category.db(),
                gap.reason.db(),
                gap.target_hint,
                gap.occurrences,
                gap.relation_site
            ],
        )
        .map_err(db_error)?;
    }
    Ok(EvidenceStats {
        generated_files,
        artifacts: evidence.artifacts.len(),
        provenance_links: inserted_links,
        runs: evidence.runs.len(),
        regions: evidence.regions.len(),
        branches: evidence.branches.len(),
        gaps: evidence.gaps.len(),
    })
}

fn validate_evidence_span(span: EvidenceLineSpan) -> Result<()> {
    if span.start == 0 || span.end < span.start {
        Err("evidence line span is invalid".into())
    } else {
        Ok(())
    }
}

fn evidence_file_id(tx: &Transaction<'_>, path: Option<&str>) -> Result<Option<i64>> {
    path.map(|path| {
        tx.query_row("SELECT id FROM files WHERE path=?1", [path], |row| {
            row.get(0)
        })
        .optional()
        .map_err(db_error)
    })
    .transpose()
    .map(Option::flatten)
}

fn coverage_test_mapping(
    tx: &Transaction<'_>,
    name: &str,
) -> Result<(Option<i64>, Option<GapReason>)> {
    let candidates = tx
        .prepare(
            "SELECT id FROM nodes
              WHERE kind='test' AND (name=?1 OR qualified_name=?1)
              ORDER BY id LIMIT 2",
        )
        .map_err(db_error)?
        .query_map([name], |row| row.get::<_, i64>(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    Ok(match candidates.as_slice() {
        [id] => (Some(*id), None),
        [] => (None, Some(GapReason::MissingTestContext)),
        _ => (None, Some(GapReason::AmbiguousTestContext)),
    })
}

fn validate_coverage_range(
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
) -> Result<()> {
    if start_line == 0 || end_line == 0 || (end_line, end_column) < (start_line, start_column) {
        Err("coverage range is invalid".into())
    } else {
        Ok(())
    }
}

fn validate_coverage_branch(
    start_line: i64,
    start_column: u32,
    end_line: i64,
    end_column: u32,
    target_line: Option<i64>,
    kind: CoverageBranchKind,
) -> Result<(u32, u32)> {
    if kind != CoverageBranchKind::Arc {
        if target_line.is_some() {
            return Err("coverage branch target is invalid".into());
        }
        let start =
            u32::try_from(start_line).map_err(|_| "coverage branch range is invalid".to_owned())?;
        let end =
            u32::try_from(end_line).map_err(|_| "coverage branch range is invalid".to_owned())?;
        validate_coverage_range(start, start_column, end, end_column)?;
        return Ok((start, end));
    }
    let target = target_line.ok_or_else(|| "coverage branch target is invalid".to_owned())?;
    if start_line == 0
        || target == 0
        || end_line != start_line
        || start_column != 0
        || end_column != 0
    {
        return Err("coverage branch range is invalid".into());
    }
    let mut lines = [start_line, target]
        .into_iter()
        .filter(|line| *line > 0)
        .map(|line| {
            u32::try_from(line).map_err(|_| "coverage branch line exceeds range".to_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    lines.sort_unstable();
    match lines.as_slice() {
        [] => Err("coverage branch range is invalid".into()),
        [line] => Ok((*line, *line)),
        [start, end] => Ok((*start, *end)),
        _ => unreachable!("an arc has exactly two endpoints"),
    }
}

fn validate_coverage_label(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        Err("coverage label is invalid".into())
    } else {
        Ok(())
    }
}

fn record_coverage_mapping_gap(
    tx: &Transaction<'_>,
    run_id: i64,
    file_id: Option<i64>,
    path: Option<&str>,
    start_line: u32,
    end_line: u32,
) -> Result<()> {
    let Some(file_id) = file_id else {
        if path.is_some() {
            insert_coverage_gap(
                tx,
                run_id,
                path,
                Some(start_line),
                Some(end_line),
                GapReason::CoverageUnmappedFile,
                None,
                1,
            )?;
        }
        return Ok(());
    };
    let mapped: bool = tx
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM nodes
                  WHERE file_id=?1 AND kind!='file'
                    AND line_start<=?2 AND line_end>=?3
             )",
            params![file_id, end_line, start_line],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if !mapped {
        insert_coverage_gap(
            tx,
            run_id,
            path,
            Some(start_line),
            Some(end_line),
            GapReason::CoverageUnmappedRegion,
            None,
            1,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_coverage_gap(
    tx: &Transaction<'_>,
    run_id: i64,
    path: Option<&str>,
    line_start: Option<u32>,
    line_end: Option<u32>,
    reason: GapReason,
    target_hint: Option<&str>,
    occurrences: u32,
) -> Result<()> {
    tx.execute(
        "INSERT INTO graph_gaps(
            file_id, source_id, run_id, path, line_start, line_end, category, reason,
            target_hint, occurrences, relation_site
         ) VALUES(NULL, NULL, ?1, ?2, ?3, ?4, 'coverage', ?5, ?6, ?7, 0)
         ON CONFLICT DO UPDATE
             SET occurrences=graph_gaps.occurrences+excluded.occurrences",
        params![
            run_id,
            path,
            line_start,
            line_end,
            reason.db(),
            target_hint,
            occurrences,
        ],
    )
    .map_err(db_error)?;
    Ok(())
}

fn refresh_script_export_methods(tx: &Transaction<'_>, cancelled: &AtomicBool) -> Result<()> {
    tx.execute(
        "DELETE FROM node_keys WHERE key LIKE 'script:export-method:%'",
        [],
    )
    .map_err(db_error)?;

    let owner_keys = tx
        .prepare(
            "SELECT key FROM (
                 SELECT key FROM node_keys WHERE key LIKE 'script:export-value:%'
                 UNION
                 SELECT alias_key AS key FROM refs
                  WHERE alias_key LIKE 'script:export-value:%'
             ) ORDER BY key",
        )
        .map_err(db_error)?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;

    let mut candidates = tx
        .prepare("SELECT node_id FROM node_keys WHERE key=?1 ORDER BY node_id LIMIT 2")
        .map_err(db_error)?;
    let mut alias_candidates = tx
        .prepare(
            "SELECT count(*), coalesce(sum(resolution_state='ambiguous'),0),
                    count(DISTINCT resolved_target_id), min(resolved_target_id)
               FROM refs WHERE alias_key=?1",
        )
        .map_err(db_error)?;
    let mut is_class = tx
        .prepare(
            "SELECT EXISTS(
                 SELECT 1 FROM node_keys
                  WHERE node_id=?1 AND key LIKE 'script:class:%'
             )",
        )
        .map_err(db_error)?;
    let mut methods = tx
        .prepare(
            "SELECT method.id, method.name FROM nodes method
              WHERE method.parent_id=?1
                AND EXISTS(
                    SELECT 1 FROM node_keys key
                     WHERE key.node_id=method.id AND key.key LIKE 'script:static-method:%'
                )
              ORDER BY method.id",
        )
        .map_err(db_error)?;
    let mut insert = tx
        .prepare("INSERT INTO node_keys(key, node_id) VALUES(?1, ?2)")
        .map_err(db_error)?;
    for owner_key in owner_keys {
        check_cancelled(cancelled)?;
        let DbCandidate::Unique(owner) = merge_candidates(
            candidate(&mut candidates, &owner_key)?,
            alias_candidate(&mut alias_candidates, &owner_key)?,
        ) else {
            continue;
        };
        if !is_class
            .query_row([owner], |row| row.get::<_, bool>(0))
            .map_err(db_error)?
        {
            continue;
        }
        let Some(owner_key) = owner_key.strip_prefix("script:export-value:") else {
            continue;
        };
        for row in methods
            .query_map([owner], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_error)?
        {
            let (method, name) = row.map_err(db_error)?;
            insert
                .execute(params![
                    format!("script:export-method:{owner_key}::{name}"),
                    method
                ])
                .map_err(db_error)?;
        }
    }

    let references = tx
        .prepare(
            "SELECT DISTINCT ref.id FROM refs ref
               JOIN ref_keys key ON key.ref_id=ref.id
              WHERE key.key LIKE 'script:export-method:%'
              ORDER BY ref.id",
        )
        .map_err(db_error)?
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<HashSet<_>>>()
        .map_err(db_error)?;
    resolve_references(tx, references, cancelled)
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
            "SELECT r.kind, n.kind, r.alias_key, r.resolved_target_id, r.resolution_state
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
            "SELECT count(*), coalesce(sum(resolution_state='ambiguous'),0),
                    count(DISTINCT resolved_target_id), min(resolved_target_id)
               FROM refs WHERE alias_key=?1",
        )
        .map_err(db_error)?;
    let mut update_ref = tx
        .prepare("UPDATE refs SET resolved_target_id=?1, resolution_state=?2 WHERE id=?3")
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
        let Some((ref_kind, source_kind, alias_key, old_target, old_state)) = load_ref
            .query_row([reference_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .optional()
            .map_err(db_error)?
        else {
            continue;
        };
        let mut outcome = DbCandidate::Missing;
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
                    outcome = DbCandidate::Unique(target);
                    break;
                }
                DbCandidate::Ambiguous => {
                    outcome = DbCandidate::Ambiguous;
                    break;
                }
                DbCandidate::Missing => {}
            }
        }
        let (new_target, new_state) = match outcome {
            DbCandidate::Unique(target) => (Some(target), ResolutionState::Resolved),
            DbCandidate::Missing => (None, ResolutionState::Missing),
            DbCandidate::Ambiguous => (None, ResolutionState::Ambiguous),
        };
        if old_target == new_target && old_state == new_state.db() {
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
            .execute(params![new_target, new_state.db(), reference_id])
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
            "SELECT count(*), coalesce(sum(r.resolution_state='ambiguous'),0),
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
    let (_total, ambiguous, distinct, target) = statement
        .query_row([key], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })
        .map_err(db_error)?;
    Ok(if ambiguous != 0 {
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
                    path, language, git_oid, content_hash, parse_context, byte_size,
                    observed_relation_sites
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
                    byte_size,
                    file.observed_relation_sites
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
                "INSERT INTO refs(
                    source_id, kind, line, alias_key, resolved_target_id, resolution_state
                 ) VALUES(?1, ?2, ?3, ?4, NULL, 'pending')",
            )
            .map_err(db_error)?;
        let mut finish_ref = tx
            .prepare("UPDATE refs SET resolved_target_id=?1, resolution_state=?2 WHERE id=?3")
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
                    reference.alias_key
                ])
                .map_err(db_error)?;
            let reference_id = tx.last_insert_rowid();
            if delta {
                if reference.resolution != ResolutionState::Pending || target_id.is_some() {
                    return Err("incremental graph contains resolved references".into());
                }
                reference_ids.push(reference_id);
            } else {
                if (reference.resolution == ResolutionState::Resolved) != target_id.is_some()
                    || reference.resolution == ResolutionState::Pending
                {
                    return Err("full graph reference resolution is invalid".into());
                }
                finish_ref
                    .execute(params![target_id, reference.resolution.db(), reference_id])
                    .map_err(db_error)?;
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
        let mut insert = tx
            .prepare(
                "INSERT INTO modeled_sites(
                    file_id, source_id, kind, line_start, line_end, target_hint, parse_context
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(db_error)?;
        for site in &graph.modeled_sites {
            check_cancelled(cancelled)?;
            let (file_id, _) = files
                .get(site.file_key.as_str())
                .ok_or_else(|| "modeled site references an unknown file".to_owned())?;
            let source_id = site
                .source_key
                .as_ref()
                .map(|key| lookup(&nodes, key, "modeled site source"))
                .transpose()?;
            insert
                .execute(params![
                    file_id,
                    source_id,
                    site.kind.db(),
                    site.line_start,
                    site.line_end,
                    site.target_hint,
                    site.parse_context
                ])
                .map_err(db_error)?;
        }
    }

    {
        let mut run = tx
            .prepare("SELECT id FROM coverage_runs WHERE key=?1")
            .map_err(db_error)?;
        let mut insert = tx
            .prepare(
                "INSERT INTO graph_gaps(
                    file_id, source_id, run_id, path, line_start, line_end, category, reason,
                    target_hint, occurrences, relation_site
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT DO UPDATE
                     SET occurrences=graph_gaps.occurrences+excluded.occurrences",
            )
            .map_err(db_error)?;
        for gap in &graph.gaps {
            check_cancelled(cancelled)?;
            let file_id = gap
                .file_key
                .as_ref()
                .map(|key| {
                    files
                        .get(key.as_str())
                        .map(|(id, _)| *id)
                        .ok_or_else(|| "gap references an unknown file".to_owned())
                })
                .transpose()?;
            let source_id = gap
                .source_key
                .as_ref()
                .map(|key| lookup(&nodes, key, "gap source"))
                .transpose()?;
            let run_id = gap
                .run_key
                .as_ref()
                .map(|key| {
                    run.query_row([key], |row| row.get::<_, i64>(0))
                        .optional()
                        .map_err(db_error)?
                        .ok_or_else(|| "gap references an unknown coverage run".to_owned())
                })
                .transpose()?;
            insert
                .execute(params![
                    file_id,
                    source_id,
                    run_id,
                    gap.path,
                    gap.line_start,
                    gap.line_end,
                    gap.category.db(),
                    gap.reason.db(),
                    gap.target_hint,
                    gap.occurrences,
                    gap.relation_site
                ])
                .map_err(db_error)?;
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
            byte_size INTEGER NOT NULL CHECK(byte_size>=0),
            observed_relation_sites INTEGER NOT NULL CHECK(observed_relation_sites>=0)
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
            resolved_target_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
            resolution_state TEXT NOT NULL
                CHECK(resolution_state IN ('pending','resolved','missing','ambiguous')),
            CHECK((resolution_state='resolved') = (resolved_target_id IS NOT NULL))
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
         CREATE TABLE modeled_sites(
            id INTEGER PRIMARY KEY,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            source_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
            kind TEXT NOT NULL
                CHECK(kind IN ('generated-inclusion','test-registration','static-export')),
            line_start INTEGER NOT NULL CHECK(line_start>0),
            line_end INTEGER NOT NULL CHECK(line_end>=line_start),
            target_hint TEXT,
            parse_context TEXT
         );
         CREATE INDEX modeled_sites_file_lines
             ON modeled_sites(file_id, line_start, line_end, kind, id);
         CREATE TABLE imported_artifacts(
            id INTEGER PRIMARY KEY,
            key TEXT NOT NULL UNIQUE CHECK(length(key)>0),
            role TEXT NOT NULL CHECK(role IN (
                'manifest','input','generated-rust','coverage-report'
            )),
            path TEXT NOT NULL CHECK(length(path)>0),
            content_hash BLOB NOT NULL CHECK(length(content_hash)=32),
            byte_size INTEGER NOT NULL CHECK(byte_size>=0),
            UNIQUE(role, path)
         );
         CREATE TABLE provenance_links(
            id INTEGER PRIMARY KEY,
            input_artifact_id INTEGER NOT NULL
                REFERENCES imported_artifacts(id) ON DELETE CASCADE,
            input_line_start INTEGER NOT NULL CHECK(input_line_start>0),
            input_line_end INTEGER NOT NULL CHECK(input_line_end>=input_line_start),
            generator_path TEXT NOT NULL CHECK(length(generator_path)>0),
            generator_file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
            generator_node_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
            generator_line_start INTEGER NOT NULL CHECK(generator_line_start>0),
            generator_line_end INTEGER NOT NULL CHECK(generator_line_end>=generator_line_start),
            output_artifact_id INTEGER NOT NULL
                REFERENCES imported_artifacts(id) ON DELETE CASCADE,
            output_line_start INTEGER NOT NULL CHECK(output_line_start>0),
            output_line_end INTEGER NOT NULL CHECK(output_line_end>=output_line_start),
            modeled_site_id INTEGER REFERENCES modeled_sites(id) ON DELETE CASCADE,
            mapping_state TEXT NOT NULL
                CHECK(mapping_state IN ('linked','unobserved','ambiguous')),
            CHECK((generator_file_id IS NULL)=(generator_node_id IS NULL)),
            CHECK((mapping_state='linked')=(
                generator_node_id IS NOT NULL AND modeled_site_id IS NOT NULL
            )),
            UNIQUE(
                input_artifact_id, input_line_start, input_line_end,
                generator_path, generator_line_start, generator_line_end,
                output_artifact_id, output_line_start, output_line_end
            )
         );
         CREATE INDEX provenance_links_input ON provenance_links(input_artifact_id, id);
         CREATE INDEX provenance_links_output ON provenance_links(output_artifact_id, id);
         CREATE INDEX provenance_links_generator ON provenance_links(generator_node_id, id);
         CREATE TABLE coverage_runs(
            id INTEGER PRIMARY KEY,
            key TEXT NOT NULL UNIQUE CHECK(length(key)>0),
            report_artifact_id INTEGER NOT NULL
                REFERENCES imported_artifacts(id) ON DELETE CASCADE,
            report_digest BLOB NOT NULL CHECK(length(report_digest)=32),
            format TEXT NOT NULL CHECK(format IN ('llvm','coverage_py')),
            run_label TEXT NOT NULL CHECK(length(run_label) BETWEEN 1 AND 200),
            test_name TEXT CHECK(test_name IS NULL OR length(test_name) BETWEEN 1 AND 200),
            test_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE
         );
         CREATE UNIQUE INDEX coverage_runs_identity
             ON coverage_runs(format, run_label, report_digest);
         CREATE TABLE coverage_regions(
            id INTEGER PRIMARY KEY,
            run_id INTEGER NOT NULL REFERENCES coverage_runs(id) ON DELETE CASCADE,
            file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
            path TEXT CHECK(path IS NULL OR length(path)>0),
            test_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
            start_line INTEGER NOT NULL CHECK(start_line>0),
            start_column INTEGER NOT NULL CHECK(start_column>=0),
            end_line INTEGER NOT NULL CHECK(end_line>=start_line),
            end_column INTEGER NOT NULL CHECK(end_column>=0),
            execution_count INTEGER NOT NULL CHECK(execution_count>=0),
            context TEXT CHECK(context IS NULL OR length(context) BETWEEN 1 AND 200),
            CHECK(end_line>start_line OR end_column>=start_column)
         );
         CREATE UNIQUE INDEX coverage_regions_identity ON coverage_regions(
            run_id, ifnull(path,''), start_line, start_column, end_line, end_column,
            ifnull(context,'')
         );
         CREATE INDEX coverage_regions_run_file
             ON coverage_regions(run_id, file_id, start_line, end_line, id);
         CREATE TABLE coverage_branches(
            id INTEGER PRIMARY KEY,
            run_id INTEGER NOT NULL REFERENCES coverage_runs(id) ON DELETE CASCADE,
            file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
            path TEXT CHECK(path IS NULL OR length(path)>0),
            test_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
            start_line INTEGER NOT NULL CHECK(start_line!=0),
            start_column INTEGER NOT NULL CHECK(start_column>=0),
            end_line INTEGER NOT NULL CHECK(end_line>=start_line),
            end_column INTEGER NOT NULL CHECK(end_column>=0),
            target_line INTEGER CHECK(target_line IS NULL OR target_line!=0),
            kind TEXT NOT NULL CHECK(kind IN ('true-outcome','false-outcome','arc')),
            execution_count INTEGER NOT NULL CHECK(execution_count>=0),
            CHECK(end_line>start_line OR end_column>=start_column),
            CHECK((kind='arc') = (target_line IS NOT NULL)),
            CHECK(kind='arc' OR start_line>0)
         );
         CREATE UNIQUE INDEX coverage_branches_identity ON coverage_branches(
            run_id, ifnull(path,''), start_line, start_column, end_line, end_column,
            ifnull(target_line,-1), kind
         );
         CREATE INDEX coverage_branches_run_file
             ON coverage_branches(run_id, file_id, start_line, target_line, kind, id);
         CREATE TABLE graph_gaps(
            id INTEGER PRIMARY KEY,
            file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
            source_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
            run_id INTEGER REFERENCES coverage_runs(id) ON DELETE CASCADE,
            path TEXT,
            line_start INTEGER CHECK(line_start IS NULL OR line_start>0),
            line_end INTEGER CHECK(line_end IS NULL OR line_end>=line_start),
            category TEXT NOT NULL CHECK(category IN (
                'source','parse','relation','macro','generated','coverage','language','boundary'
            )),
            reason TEXT NOT NULL CHECK(reason IN (
                'unsafe-path','non-regular','unmerged','oversized','invalid-utf8',
                'missing-during-read','parser-error','parser-no-tree',
                'dynamic-or-unsupported-dispatch','macro-expansion-unavailable',
                'generated-output-unobserved','generated-output-ambiguous',
                'external-dependency','dependency-collapsed','language-not-indexed',
                'coverage-unmapped-file','coverage-unmapped-region','missing-test-context',
                'ambiguous-test-context'
            )),
            target_hint TEXT,
            occurrences INTEGER NOT NULL CHECK(occurrences>0),
            relation_site INTEGER NOT NULL CHECK(relation_site IN (0,1)),
            CHECK(line_end IS NULL OR line_start IS NOT NULL)
         );
         CREATE UNIQUE INDEX graph_gaps_identity ON graph_gaps(
            ifnull(file_id,-1), ifnull(source_id,-1), ifnull(run_id,-1), ifnull(path,''),
            ifnull(line_start,-1), ifnull(line_end,-1), category, reason,
            ifnull(target_hint,''), relation_site
         );
         CREATE INDEX graph_gaps_order ON graph_gaps(
            path, line_start, line_end, category, reason, target_hint, id
         );
         CREATE VIRTUAL TABLE nodes_fts
             USING fts5(name, qualified_name, path, signature);
         PRAGMA user_version=8;",
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

    fn canonical_temp_dir() -> PathBuf {
        // Test-only helper creates no shared temporary entry.
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        fs::canonicalize(std::env::temp_dir()).expect("temporary directory must resolve")
    }

    #[test]
    fn resolution_state_resolved_missing_and_ambiguous_own_exact_edges() {
        for (targets, state, edges) in [
            (1, "resolved", 1_i64),
            (0, "missing", 0),
            (2, "ambiguous", 0),
        ] {
            let mut store = Store {
                connection: Connection::open_in_memory().unwrap(),
            };
            store
                .index_with(&AtomicBool::new(false), |_full, _existing| {
                    Ok((resolution_graph(targets), ()))
                })
                .unwrap();

            let (actual_state, target): (String, Option<i64>) = store
                .connection
                .query_row(
                    "SELECT resolution_state, resolved_target_id FROM refs",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(actual_state, state);
            assert_eq!(target.is_some(), state == "resolved");
            assert_eq!(
                store
                    .connection
                    .query_row("SELECT count(*) FROM edges", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                edges
            );
        }
    }

    #[test]
    fn incremental_resolution_state_changes_with_candidates() {
        let cancelled = AtomicBool::new(false);
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((resolution_graph(0), ())))
            .unwrap();
        assert_ref_state(&store, "missing", 0);

        for (targets, state, edges) in [
            (1, "resolved", 1_i64),
            (2, "ambiguous", 0),
            (1, "resolved", 1),
        ] {
            let graph = incremental_resolution_graph(targets);
            store
                .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
                .unwrap();
            assert_ref_state(&store, state, edges);
        }
    }

    #[test]
    fn incremental_global_gaps_replace_the_complete_inventory() {
        let cancelled = AtomicBool::new(false);
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((global_gap_graph(GapReason::UnsafePath, 2), ()))
            })
            .unwrap();

        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((global_gap_graph(GapReason::Oversized, 1), ()))
            })
            .unwrap();
        assert_eq!(stored_global_gaps(&store), [("oversized".into(), 1)]);

        store
            .index_with(&cancelled, |_full, _existing| Ok((Graph::default(), ())))
            .unwrap();
        assert!(stored_global_gaps(&store).is_empty());
    }

    #[test]
    fn resolution_state_constraints_and_seal_reject_invalid_rows() {
        let root = canonical_temp_dir().join(format!(
            "graphr-resolution-state-{}-{}",
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
            .index_with(&cancelled, |_full, _existing| Ok((resolution_graph(1), ())))
            .unwrap();

        assert!(
            store
                .connection
                .execute(
                    "UPDATE refs SET resolution_state='resolved', resolved_target_id=NULL",
                    [],
                )
                .is_err()
        );
        assert!(
            store
                .connection
                .execute(
                    "UPDATE refs SET resolution_state='missing' WHERE resolved_target_id IS NOT NULL",
                    [],
                )
                .is_err()
        );
        store
            .connection
            .execute(
                "UPDATE refs SET resolution_state='pending', resolved_target_id=NULL",
                [],
            )
            .unwrap();
        assert_eq!(
            store.seal(&cancelled).unwrap_err(),
            "database contains pending references"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn seal_recomputes_reference_candidate_resolution() {
        let error = sealed_resolution_corruption("candidate", |connection| {
            connection
                .execute(
                    "UPDATE refs SET resolved_target_id=NULL, resolution_state='missing'",
                    [],
                )
                .unwrap();
        });

        assert_eq!(
            error,
            "database reference resolution does not match candidates"
        );
    }

    #[test]
    fn seal_verifies_resolved_reference_edge_ownership_and_support() {
        for (label, sql) in [
            ("missing-edge", "DELETE FROM edges"),
            ("wrong-support", "UPDATE edges SET support_count=2"),
            ("wrong-owner", "UPDATE edges SET kind='IMPORTS'"),
        ] {
            let error = sealed_resolution_corruption(label, |connection| {
                connection.execute(sql, []).unwrap();
            });
            assert_eq!(
                error, "database reference edges do not match resolved references",
                "{label}"
            );
        }
    }

    #[test]
    fn seal_and_image_validation_reject_surplus_unowned_edges() {
        let insert_surplus = "INSERT INTO edges(source_id, target_id, kind, support_count)
                              SELECT source_id, resolved_target_id, 'IMPORTS', 1 FROM refs";
        let seal_error = sealed_resolution_corruption("surplus-edge", |connection| {
            connection.execute(insert_surplus, []).unwrap();
        });
        assert_eq!(
            seal_error,
            "database reference edges do not match resolved references"
        );

        let image_error = sealed_image_corruption("surplus-edge", |connection| {
            connection.execute(insert_surplus, []).unwrap();
        });
        assert_eq!(
            image_error,
            "database reference edges do not match resolved references"
        );
    }

    #[test]
    fn reference_candidate_validation_observes_midpass_cancellation() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&AtomicBool::new(false), |_full, _existing| {
                Ok((resolution_graph(1), ()))
            })
            .unwrap();
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        set_after_reference_candidate_pass_hook(move || cancel.store(true, Ordering::Relaxed));

        assert_eq!(
            require_graph_invariants(&store.connection, &cancelled).unwrap_err(),
            "index cancelled"
        );
    }

    #[test]
    fn relation_site_accounting_mismatch_rolls_back_publication() {
        let mut graph = single_node_graph("unaccounted");
        graph.files[0].observed_relation_sites = 1;
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };

        assert_eq!(
            store
                .index_with(&AtomicBool::new(false), |_full, _existing| Ok((graph, ())))
                .unwrap_err(),
            "observed relation-site accounting mismatch for src/lib.rs"
        );
        assert_eq!(
            store
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn gap_ownership_folds_identical_rows_and_cascades_on_replacement() {
        let cancelled = AtomicBool::new(false);
        let mut graph = single_node_graph("owned");
        graph.modeled_sites.push(ModeledSiteInput {
            file_key: "src/lib.rs".into(),
            source_key: Some("owned".into()),
            kind: ModeledSiteKind::StaticExport,
            line_start: 1,
            line_end: 1,
            target_hint: Some("owned".into()),
            parse_context: None,
        });
        graph.gaps.extend([2, 3].map(|occurrences| GapInput {
            file_key: Some("src/lib.rs".into()),
            source_key: Some("owned".into()),
            run_key: None,
            path: Some("src/lib.rs".into()),
            line_start: Some(1),
            line_end: Some(1),
            category: GapCategory::Parse,
            reason: GapReason::ParserError,
            target_hint: None,
            occurrences,
            relation_site: false,
        }));
        graph.files[0].observed_relation_sites = 1;
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        assert_eq!(
            store
                .connection
                .query_row("SELECT occurrences FROM graph_gaps", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            5
        );

        let graph = single_node_graph("replacement");
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        for table in ["modeled_sites", "graph_gaps"] {
            assert_eq!(
                store
                    .connection
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn sealed_image_is_single_file_and_read_only() {
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!(
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
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!(
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
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!(
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
    fn validate_image_rejects_invalid_file_language() {
        let error = sealed_image_corruption("file-language", |connection| {
            connection
                .execute("UPDATE files SET language='go'", [])
                .unwrap();
        });

        assert_eq!(error, "database file language is invalid");
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
        let path = canonical_temp_dir().join(format!(
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
        let mut graph = Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
                observed_relation_sites: 0,
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
        own_graph_edges(&mut graph);
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
        own_graph_edges(&mut graph);
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
            .unwrap()
            .graph;
        assert!(output.contains(" n6 "), "{output}");
        assert!(!output.contains(" n7 "), "{output}");
        assert!(output.contains("neighborhood_omitted=false"), "{output}");
        assert!(output.contains("static_model=complete"), "{output}");
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
        let mut graph = Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [0; 32],
                parse_context: String::new(),
                byte_size: 64,
                replace: true,
                observed_relation_sites: 0,
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
        own_graph_edges(&mut graph);
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
            .unwrap()
            .graph;
        assert!(
            output.contains(
                "risk overall=0.4200 changed_symbols_total=1 changed_symbols_analyzed=1 changed_symbols_emitted=1 changed_symbols_omitted=0 flows_discovered=1 flows_total=unknown static_test_path_gaps=0 traversal_complete=false analysis_roots_omitted=0 deleted_paths_unanalyzed=1 neighborhood_omitted=false"
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
            .unwrap()
            .graph;
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
            .unwrap()
            .graph;
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
            .unwrap()
            .graph;
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
            .unwrap()
            .graph;
        assert!(output.contains(" root "), "{output}");
        assert!(output.contains("test <-"), "{output}");
        assert!(output.contains("caller <-"), "{output}");
        assert!(
            output.contains("unmapped src/untracked-499.rs:1"),
            "{output}"
        );
        assert!(output.contains("static_model=partial"), "{output}");
        assert!(
            output.contains(
                "claim kind=affected-flows status=partial basis=resolved-static-call-graph"
            )
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
        own_graph_edges(&mut graph);
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
                observed_relation_sites: 0,
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
            .unwrap()
            .graph;
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
            .unwrap()
            .graph;
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
            .unwrap()
            .graph;
        assert!(output.contains("file-mapped src/lib.rs:1-7"), "{output}");
        assert!(!output.contains("file-mapped src/lib.rs:1\n"), "{output}");
    }

    #[test]
    fn deleted_only_changes_report_incomplete_analysis() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&AtomicBool::new(false), |_full, _existing| {
                Ok((Graph::default(), ()))
            })
            .unwrap();
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
            .unwrap()
            .graph;
        assert!(
            output.contains("flows_discovered=0 flows_total=unknown"),
            "{output}"
        );
        assert!(output.contains("traversal_complete=false"), "{output}");
        assert!(output.contains("deleted_paths_unanalyzed=1"), "{output}");
        assert!(output.contains(
            "claim kind=affected-callers status=partial basis=resolved-static-call-graph"
        ));
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
                observed_relation_sites: 0,
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
            .unwrap()
            .graph;

        assert!(output.contains(" Function first src/lib.rs:2"), "{output}");
        assert!(output.contains(" Function second src/lib.rs:6"), "{output}");
        assert!(output.contains("file-mapped src/lib.rs:1,5,9"), "{output}");
        assert!(!output.contains("file-mapped src/lib.rs:1-9"), "{output}");
        assert!(output.contains("static_model=complete"), "{output}");
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
                observed_relation_sites: 0,
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
            .unwrap()
            .graph;

        assert!(
            output.contains(
                "changed_symbols_total=51 changed_symbols_analyzed=51 changed_symbols_emitted=51 changed_symbols_omitted=0 flows_total=0 static_test_path_gaps=51 traversal_complete=true analysis_roots_omitted=0 deleted_paths_unanalyzed=0"
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
        own_graph_edges(&mut graph);
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
            .unwrap()
            .graph;

        assert!(output.contains("changed_symbols_total=43"), "{output}");
        assert!(output.contains("neighbor_7 src/lib.rs:107"), "{output}");
        assert!(output.contains("neighborhood_omitted=false"), "{output}");
        assert!(output.contains("static_model=complete"), "{output}");
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
                observed_relation_sites: 0,
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
            .unwrap()
            .graph;

        assert!(
            output.contains(
                "changed_symbols_total=501 changed_symbols_analyzed=500 changed_symbols_emitted=500 changed_symbols_omitted=1 flows_discovered=0 flows_total=unknown"
            ),
            "{output}"
        );
        assert!(output.contains("traversal_complete=false"), "{output}");
        assert!(output.contains("analysis_roots_omitted=1"), "{output}");
        assert!(output.contains("neighborhood_omitted=true"), "{output}");
        assert!(output.contains(
            "claim kind=static-test-paths status=partial basis=resolved-static-call-graph"
        ));
    }

    #[test]
    fn reparents_cpp_methods_when_the_owner_type_is_added_incrementally() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let cancelled = AtomicBool::new(false);
        let file = |path: &str, replace| FileInput {
            path: path.into(),
            language: Language::Cpp,
            git_oid: None,
            content_hash: [0; 32],
            parse_context: String::new(),
            byte_size: 1,
            replace,
            observed_relation_sites: 0,
        };
        let file_node = |key: &str, path: &str| NodeInput {
            key: key.into(),
            file_key: path.into(),
            kind: NodeKind::File,
            name: path.into(),
            qualified_name: key.into(),
            parent_key: None,
            owner_key: None,
            line_start: 1,
            line_end: 1,
            signature: String::new(),
            keys: vec![],
        };

        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((
                    Graph {
                        files: vec![file("src/method.cpp", true)],
                        nodes: vec![
                            file_node("method-file", "src/method.cpp"),
                            NodeInput {
                                key: "run".into(),
                                file_key: "src/method.cpp".into(),
                                kind: NodeKind::Function,
                                name: "run".into(),
                                qualified_name: "run".into(),
                                parent_key: Some("method-file".into()),
                                owner_key: Some("cpp:item:Worker".into()),
                                line_start: 1,
                                line_end: 1,
                                signature: String::new(),
                                keys: vec!["cpp:item:Worker::run".into()],
                            },
                        ],
                        ..Graph::default()
                    },
                    (),
                ))
            })
            .unwrap();

        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((
                    Graph {
                        files: vec![file("src/method.cpp", false), file("src/worker.h", true)],
                        nodes: vec![
                            file_node("worker-file", "src/worker.h"),
                            NodeInput {
                                key: "worker".into(),
                                file_key: "src/worker.h".into(),
                                kind: NodeKind::Type,
                                name: "Worker".into(),
                                qualified_name: "Worker".into(),
                                parent_key: Some("worker-file".into()),
                                owner_key: None,
                                line_start: 1,
                                line_end: 1,
                                signature: String::new(),
                                keys: vec!["cpp:item:Worker".into()],
                            },
                        ],
                        ..Graph::default()
                    },
                    (),
                ))
            })
            .unwrap();

        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT parent.kind, parent.name FROM nodes method
                       JOIN nodes parent ON parent.id=method.parent_id
                      WHERE method.name='run'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            ("type".into(), "Worker".into())
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
            observed_relation_sites: 0,
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
            .unwrap()
            .graph;
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
    fn provenance_replacement_is_atomic_and_unique() {
        let cancelled = AtomicBool::new(false);
        let mut source = single_node_graph("generator");
        source.modeled_sites.push(ModeledSiteInput {
            file_key: "src/lib.rs".into(),
            source_key: Some("generator".into()),
            kind: ModeledSiteKind::GeneratedInclusion,
            line_start: 1,
            line_end: 1,
            target_hint: Some("out.rs".into()),
            parse_context: Some("0:".into()),
        });
        source.files[0].observed_relation_sites = 1;
        source.gaps.push(GapInput {
            file_key: None,
            source_key: None,
            run_key: None,
            path: Some("schema.proto".into()),
            line_start: None,
            line_end: None,
            category: GapCategory::Language,
            reason: GapReason::LanguageNotIndexed,
            target_hint: None,
            occurrences: 1,
            relation_site: false,
        });
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let (source_state, _, ()) = store
            .index_with(&cancelled, |_full, _existing| Ok((source, ())))
            .unwrap();
        let evidence = EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                imported("input", "schema.proto", ArtifactRole::Input, 2),
                imported("output", "target/out.rs", ArtifactRole::GeneratedRust, 3),
            ],
            provenance: vec![ProvenanceInput {
                input_key: "input".into(),
                input_lines: EvidenceLineSpan { start: 1, end: 1 },
                generator_path: "src/lib.rs".into(),
                generator_lines: EvidenceLineSpan { start: 1, end: 1 },
                output_key: "output".into(),
                output_lines: EvidenceLineSpan { start: 1, end: 1 },
            }],
            ..EvidenceInput::default()
        };

        let stats = store
            .replace_evidence(generated_output_graph(), &evidence, &cancelled)
            .unwrap();
        assert_eq!(stats.artifacts, 3);
        assert_eq!(stats.provenance_links, 1);
        assert_eq!(
            read_state(&store.connection).unwrap().generation,
            source_state.generation + 1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT occurrences FROM graph_gaps WHERE path='schema.proto'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let rendered = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(rendered.evidence.contains(
            "claim kind=generated-provenance status=complete result=linked basis=verified-generated-manifest"
        ));
        assert!(rendered
            .evidence
            .contains("provenance input=\"schema.proto:1-1\" generator=\"src/lib.rs:1-1\" output=\"target/out.rs:1-1\""));
        assert!(
            rendered
                .evidence
                .contains("includes source=\"src/lib.rs:1\" output=\"target/out.rs\"")
        );
        assert_eq!(rendered.dynamic_status, CompletenessStatus::Complete);

        let collision = Graph {
            files: vec![FileInput {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [9; 32],
                parse_context: "0:".into(),
                byte_size: 1,
                replace: true,
                observed_relation_sites: 0,
            }],
            ..Graph::default()
        };
        assert!(
            store
                .replace_evidence(collision, &EvidenceInput::default(), &cancelled)
                .is_err()
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM provenance_links", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );

        let invalid = EvidenceInput {
            artifacts: vec![
                imported("duplicate", "one", ArtifactRole::Input, 1),
                imported("duplicate", "two", ArtifactRole::Input, 2),
            ],
            ..EvidenceInput::default()
        };
        assert!(
            store
                .replace_evidence(Graph::default(), &invalid, &cancelled)
                .is_err()
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM provenance_links", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn duplicate_provenance_declaration_rolls_back_the_evidence_transaction() {
        let cancelled = AtomicBool::new(false);
        let mut store = provenance_source_store(None);
        let before = read_state(&store.connection).unwrap();
        let mut evidence = provenance_evidence();
        evidence.provenance.push(evidence.provenance[0].clone());

        assert_eq!(
            store
                .replace_evidence(generated_output_graph(), &evidence, &cancelled)
                .unwrap_err(),
            "duplicate provenance declaration"
        );
        assert_eq!(read_state(&store.connection).unwrap(), before);
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM imported_artifacts", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM provenance_links", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn seal_and_image_validation_recompute_provenance_declaration_state() {
        let cancelled = AtomicBool::new(false);
        let root = canonical_temp_dir().join(format!(
            "graphr-provenance-state-validation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("graph.db");
        let mut store = provenance_source_store(Some(&path));
        store
            .replace_evidence(generated_output_graph(), &provenance_evidence(), &cancelled)
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE provenance_links
                    SET modeled_site_id=NULL, mapping_state='unobserved'",
                [],
            )
            .unwrap();
        assert_eq!(
            store.seal(&cancelled).unwrap_err(),
            "database provenance declaration mapping is inconsistent"
        );

        let store = Store::open_private_image(&path, &cancelled).unwrap();
        store
            .connection
            .execute(
                "UPDATE provenance_links
                    SET modeled_site_id=(SELECT id FROM modeled_sites), mapping_state='linked'",
                [],
            )
            .unwrap();
        store.seal(&cancelled).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE provenance_links
                    SET modeled_site_id=NULL, mapping_state='unobserved'",
                [],
            )
            .unwrap();
        drop(connection);
        assert_eq!(
            validate_image(&path).unwrap_err(),
            "database provenance declaration mapping is inconsistent"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn seal_rejects_unsafe_provenance_declaration_identity() {
        let cancelled = AtomicBool::new(false);
        let root = canonical_temp_dir().join(format!(
            "graphr-provenance-path-validation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("graph.db");
        let mut store = provenance_source_store(Some(&path));
        store
            .replace_evidence(generated_output_graph(), &provenance_evidence(), &cancelled)
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE provenance_links
                    SET generator_path='../escape.rs',
                        generator_file_id=NULL, generator_node_id=NULL,
                        mapping_state='unobserved'",
                [],
            )
            .unwrap();

        assert_eq!(
            store.seal(&cancelled).unwrap_err(),
            "database provenance declaration path is unsafe"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn coverage_mapping_maps_exact_tests_and_owns_every_gap_by_run() {
        let cancelled = AtomicBool::new(false);
        let mut graph = single_node_graph("changed");
        graph.nodes[0].line_end = 3;
        graph.nodes.extend([
            test_node("named-key", "named", 10),
            test_node("ambiguous-one", "ambiguous", 11),
            test_node("ambiguous-two", "ambiguous", 12),
        ]);
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        store
            .replace_evidence(Graph::default(), &coverage_mapping_evidence(), &cancelled)
            .unwrap();

        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT count(*) FROM coverage_runs r JOIN nodes n ON n.id=r.test_id
                      WHERE r.key='llvm-run' AND n.name='named'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT count(*) FROM coverage_regions r JOIN coverage_runs run ON run.id=r.run_id
                      JOIN nodes n ON n.id=r.test_id
                      WHERE run.key='llvm-run' AND n.name='named'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT count(*) FROM coverage_regions r JOIN coverage_runs run ON run.id=r.run_id
                      JOIN nodes n ON n.id=r.test_id
                      WHERE run.key='python-run' AND n.name='named'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT count(*) FROM coverage_branches b JOIN coverage_runs run ON run.id=b.run_id
                      WHERE run.key='python-run' AND b.test_id IS NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .prepare(
                    "SELECT reason, count(*) FROM graph_gaps
                      WHERE category='coverage' AND run_id IS NOT NULL
                      GROUP BY reason ORDER BY reason",
                )
                .unwrap()
                .query_map([], |row| Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?
                )))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap(),
            vec![
                ("ambiguous-test-context".into(), 1),
                ("coverage-unmapped-file".into(), 1),
                ("coverage-unmapped-region".into(), 1),
                ("missing-test-context".into(), 1),
            ]
        );
        let review = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                1,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(review.evidence.contains(
            "claim kind=changed-execution path=\"src/lib.rs\" lines=2 status=partial result=unknown basis=coverage-py-json run=\"python\""
        ));
        assert_eq!(review.dynamic_status, CompletenessStatus::Partial);
    }

    #[test]
    fn coverage_gap_rendering_preserves_distinct_escaped_test_contexts() {
        let cancelled = AtomicBool::new(false);
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((single_node_graph("changed"), ()))
            })
            .unwrap();
        let contexts = ["test_a", "test_\"b"];
        let evidence = EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                imported("report", "coverage.json", ArtifactRole::CoverageReport, 2),
            ],
            runs: vec![CoverageRunInput {
                key: "run".into(),
                format: CoverageFormat::CoveragePy,
                report_key: "report".into(),
                run_label: "python".into(),
                test_name: None,
            }],
            regions: contexts
                .iter()
                .map(|context| coverage_region("run", "src/lib.rs", 1, 1, 1, Some(context)))
                .collect(),
            ..EvidenceInput::default()
        };
        store
            .replace_evidence(Graph::default(), &evidence, &cancelled)
            .unwrap();

        let review = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                1,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        for context in contexts {
            assert!(
                review.evidence.contains(&format!(
                    "claim kind=changed-execution path=\"src/lib.rs\" lines=1 status=partial result=unknown basis=coverage-py-json run=\"python\" test={context:?}"
                )),
                "{}",
                review.evidence
            );
            assert!(
                review.evidence.contains(&format!(
                    "gap category=coverage reason=missing-test-context run=\"python\" path=\"src/lib.rs\" line=1 target={context:?} occurrences=1"
                )),
                "{}",
                review.evidence
            );
        }
    }

    #[test]
    fn coverage_mapping_duplicate_format_run_and_report_digest_rolls_back() {
        let cancelled = AtomicBool::new(false);
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((single_node_graph("changed"), ()))
            })
            .unwrap();
        let evidence = EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                imported("report-one", "one.json", ArtifactRole::CoverageReport, 2),
                imported("report-two", "two.json", ArtifactRole::CoverageReport, 2),
            ],
            runs: vec![
                CoverageRunInput {
                    key: "one".into(),
                    format: CoverageFormat::Llvm,
                    report_key: "report-one".into(),
                    run_label: "same".into(),
                    test_name: None,
                },
                CoverageRunInput {
                    key: "two".into(),
                    format: CoverageFormat::Llvm,
                    report_key: "report-two".into(),
                    run_label: "same".into(),
                    test_name: Some("changed".into()),
                },
            ],
            ..EvidenceInput::default()
        };

        assert!(
            store
                .replace_evidence(Graph::default(), &evidence, &cancelled)
                .is_err()
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM imported_artifacts", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM nodes", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn coverage_mapping_preserves_distinct_unmapped_relative_path_identities() {
        let cancelled = AtomicBool::new(false);
        for branches in [false, true] {
            let mut store = Store {
                connection: Connection::open_in_memory().unwrap(),
            };
            store
                .index_with(&cancelled, |_full, _existing| {
                    Ok((single_node_graph("changed"), ()))
                })
                .unwrap();
            let paths = ["missing-a.rs", "missing-b.rs"];
            let evidence = EvidenceInput {
                artifacts: vec![
                    imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                    imported("report", "report.json", ArtifactRole::CoverageReport, 2),
                ],
                runs: vec![CoverageRunInput {
                    key: "run".into(),
                    format: CoverageFormat::Llvm,
                    report_key: "report".into(),
                    run_label: "run".into(),
                    test_name: None,
                }],
                regions: if branches {
                    Vec::new()
                } else {
                    paths
                        .iter()
                        .map(|path| coverage_region("run", path, 1, 1, 1, None))
                        .collect()
                },
                branches: if branches {
                    paths
                        .iter()
                        .map(|path| CoverageBranchInput {
                            run_key: "run".into(),
                            path: Some((*path).into()),
                            start_line: 1,
                            start_column: 1,
                            end_line: 1,
                            end_column: 2,
                            target_line: None,
                            kind: CoverageBranchKind::TrueOutcome,
                            execution_count: 1,
                        })
                        .collect()
                } else {
                    Vec::new()
                },
                gaps: Vec::new(),
                provenance: Vec::new(),
            };

            store
                .replace_evidence(Graph::default(), &evidence, &cancelled)
                .unwrap();
            let table = if branches {
                "coverage_branches"
            } else {
                "coverage_regions"
            };
            let stored = store
                .connection
                .prepare(&format!("SELECT path FROM {table} ORDER BY path"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(stored, paths);

            let review = store
                .changes(
                    SNAPSHOT,
                    &WorktreeChanges {
                        files: paths
                            .iter()
                            .map(|path| ChangedFile {
                                path: (*path).into(),
                                whole_file: true,
                                spans: Vec::new(),
                                report_unmapped: true,
                            })
                            .collect(),
                        records: Vec::new(),
                        paths: Vec::new(),
                        source_patch: String::new(),
                        artifacts: Default::default(),
                        skipped_paths: 0,
                    },
                    1,
                    10,
                    DependencyMode::Boundary,
                    &cancelled,
                )
                .unwrap();
            for path in paths {
                assert!(
                    review.evidence.contains(&format!(
                        "claim kind=changed-execution path={path:?} lines=1 status=partial result=unknown"
                    )),
                    "{}",
                    review.evidence
                );
            }
        }
    }

    #[test]
    fn coverage_mapping_renders_relevant_pathless_manifest_test_gap_reasons() {
        let cancelled = AtomicBool::new(false);
        for (test_name, reason, ambiguous) in [
            ("missing", "missing-test-context", false),
            ("ambiguous", "ambiguous-test-context", true),
        ] {
            let mut graph = single_node_graph("changed");
            if ambiguous {
                graph.nodes.extend([
                    test_node("ambiguous-one", "ambiguous", 10),
                    test_node("ambiguous-two", "ambiguous", 11),
                ]);
            }
            let mut store = Store {
                connection: Connection::open_in_memory().unwrap(),
            };
            store
                .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
                .unwrap();
            let evidence = EvidenceInput {
                artifacts: vec![
                    imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                    imported("report", "report.json", ArtifactRole::CoverageReport, 2),
                ],
                runs: vec![CoverageRunInput {
                    key: "run".into(),
                    format: CoverageFormat::Llvm,
                    report_key: "report".into(),
                    run_label: "run".into(),
                    test_name: Some(test_name.into()),
                }],
                regions: vec![coverage_region("run", "src/lib.rs", 1, 1, 1, None)],
                ..EvidenceInput::default()
            };
            store
                .replace_evidence(Graph::default(), &evidence, &cancelled)
                .unwrap();

            let review = store
                .changes(
                    SNAPSHOT,
                    &changed_lib(),
                    1,
                    10,
                    DependencyMode::Boundary,
                    &cancelled,
                )
                .unwrap();
            assert!(
                review.evidence.contains(&format!(
                    "claim kind=changed-execution status=partial result=unknown basis=llvm-coverage-json run=\"run\" test={test_name:?}"
                )),
                "{}",
                review.evidence
            );
            assert!(
                review.evidence.contains(&format!(
                    "gap category=coverage reason={reason} run=\"run\" target={test_name:?} occurrences=1"
                )),
                "{}",
                review.evidence
            );
        }
    }

    #[test]
    fn changed_execution_claim_renders_scoped_counts_and_keeps_static_test_calls() {
        let cancelled = AtomicBool::new(false);
        let mut graph = single_node_graph("changed");
        graph.nodes[0].line_end = 3;
        graph.nodes.push(test_node("named-key", "named", 10));
        graph.edges.push(EdgeInput {
            source_key: "named-key".into(),
            target_key: "changed".into(),
            kind: EdgeKind::TestCalls,
            support_count: 1,
        });
        own_graph_edges(&mut graph);
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        let evidence = EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                imported(
                    "positive-report",
                    "positive.json",
                    ArtifactRole::CoverageReport,
                    2,
                ),
                imported("zero-report", "zero.json", ArtifactRole::CoverageReport, 3),
                imported(
                    "aggregate-positive-report",
                    "aggregate-positive.json",
                    ArtifactRole::CoverageReport,
                    4,
                ),
            ],
            runs: vec![
                CoverageRunInput {
                    key: "positive".into(),
                    format: CoverageFormat::Llvm,
                    report_key: "positive-report".into(),
                    run_label: "positive".into(),
                    test_name: Some("named".into()),
                },
                CoverageRunInput {
                    key: "zero".into(),
                    format: CoverageFormat::Llvm,
                    report_key: "zero-report".into(),
                    run_label: "zero".into(),
                    test_name: None,
                },
                CoverageRunInput {
                    key: "aggregate-positive".into(),
                    format: CoverageFormat::Llvm,
                    report_key: "aggregate-positive-report".into(),
                    run_label: "aggregate-positive".into(),
                    test_name: None,
                },
            ],
            regions: vec![
                coverage_region("positive", "src/lib.rs", 1, 3, 2, None),
                coverage_region("zero", "src/lib.rs", 1, 3, 0, None),
                coverage_region("aggregate-positive", "src/lib.rs", 1, 3, 2, None),
            ],
            branches: vec![CoverageBranchInput {
                run_key: "positive".into(),
                path: Some("src/lib.rs".into()),
                start_line: 2,
                start_column: 1,
                end_line: 2,
                end_column: 2,
                target_line: None,
                kind: CoverageBranchKind::TrueOutcome,
                execution_count: 1,
            }],
            ..EvidenceInput::default()
        };
        store
            .replace_evidence(Graph::default(), &evidence, &cancelled)
            .unwrap();

        let review = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                1,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(review.graph.contains("execution_mapping=complete"));
        assert!(review.evidence.contains(
            "claim kind=changed-execution path=\"src/lib.rs\" lines=1-3 status=complete result=observed basis=llvm-coverage-json run=\"positive\" test=\"named\""
        ));
        assert!(review.evidence.contains(
            "claim kind=changed-execution path=\"src/lib.rs\" lines=1-3 status=complete result=not-observed basis=llvm-coverage-json run=\"zero\""
        ));
        assert!(review.evidence.contains(
            "claim kind=changed-execution path=\"src/lib.rs\" lines=1-3 status=complete result=observed basis=llvm-coverage-json run=\"aggregate-positive\""
        ));
        assert!(!review.evidence.contains("run=\"zero\" test="));
        assert!(!review.evidence.contains("run=\"aggregate-positive\" test="));
        assert!(
            review
                .evidence
                .contains("not-observed run=\"zero\" path=\"src/lib.rs\" lines=1-3 count=0")
        );
        assert!(review.evidence.contains(
            "observed-branch run=\"positive\" test=\"named\" path=\"src/lib.rs\" line=2 arm=true count=1"
        ));
        let named_branch = review
            .evidence
            .find("observed-branch run=\"positive\"")
            .unwrap();
        let run_level = review.evidence.find("not-observed run=\"zero\"").unwrap();
        assert!(named_branch < run_level, "{}", review.evidence);
        assert_eq!(review.dynamic_status, CompletenessStatus::Complete);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT count(*) FROM edges WHERE kind='TEST_CALLS'",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn coverage_py_signed_arcs_round_trip_through_store_render_and_seal() {
        let cancelled = AtomicBool::new(false);
        let root = canonical_temp_dir().join(format!(
            "graphr-signed-coverage-arcs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("graph.db");
        let mut graph = single_node_graph("changed");
        graph.nodes[0].line_end = 8;
        let mut store = Store::open_private_image(&path, &cancelled).unwrap();
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        let evidence = EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                imported("report", "report.json", ArtifactRole::CoverageReport, 2),
            ],
            runs: vec![CoverageRunInput {
                key: "run".into(),
                format: CoverageFormat::CoveragePy,
                report_key: "report".into(),
                run_label: "run".into(),
                test_name: None,
            }],
            branches: [(-1, 8), (8, -1)]
                .into_iter()
                .map(|(start, target)| CoverageBranchInput {
                    run_key: "run".into(),
                    path: Some("src/lib.rs".into()),
                    start_line: start,
                    start_column: 0,
                    end_line: start,
                    end_column: 0,
                    target_line: Some(target),
                    kind: CoverageBranchKind::Arc,
                    execution_count: 1,
                })
                .collect(),
            ..EvidenceInput::default()
        };

        store
            .replace_evidence(Graph::default(), &evidence, &cancelled)
            .unwrap();
        let review = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                1,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(review.evidence.contains(
            "observed-branch run=\"run\" path=\"src/lib.rs\" line=-1 arm=target:8 count=1"
        ));
        assert!(review.evidence.contains(
            "observed-branch run=\"run\" path=\"src/lib.rs\" line=8 arm=target:-1 count=1"
        ));
        store.seal(&cancelled).unwrap();
        validate_image(&path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_coverage_gap_is_anonymous_partial_in_changes_and_view() {
        let cancelled = AtomicBool::new(false);
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| {
                Ok((single_node_graph("changed"), ()))
            })
            .unwrap();
        let evidence = EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                imported("report", "report.json", ArtifactRole::CoverageReport, 2),
            ],
            runs: vec![CoverageRunInput {
                key: "run".into(),
                format: CoverageFormat::Llvm,
                report_key: "report".into(),
                run_label: "external-only".into(),
                test_name: None,
            }],
            gaps: vec![GapInput {
                file_key: None,
                source_key: None,
                run_key: Some("run".into()),
                path: None,
                line_start: None,
                line_end: None,
                category: GapCategory::Coverage,
                reason: GapReason::CoverageUnmappedFile,
                target_hint: None,
                occurrences: 2,
                relation_site: false,
            }],
            ..EvidenceInput::default()
        };
        store
            .replace_evidence(Graph::default(), &evidence, &cancelled)
            .unwrap();

        let changes = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                1,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert_eq!(changes.dynamic_status, CompletenessStatus::Partial);
        assert!(changes.evidence.contains(
            "claim kind=changed-execution status=partial result=unknown basis=llvm-coverage-json run=\"external-only\""
        ));
        assert!(changes.evidence.contains(
            "gap category=coverage reason=coverage-unmapped-file run=\"external-only\" occurrences=2"
        ));
        assert!(!changes.evidence.contains("secret/external.rs"));

        let state = read_state(&store.connection).unwrap();
        let node_id = store
            .connection
            .query_row("SELECT id FROM nodes WHERE name='changed'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        let view = store
            .view(
                SNAPSHOT,
                &format!(
                    "n1:{SNAPSHOT}:{}:{}:{node_id}",
                    state.epoch, state.generation
                ),
                0,
                10,
            )
            .unwrap();
        assert!(view.contains("execution_mapping=partial"), "{view}");
        assert!(view.contains("reason=coverage-unmapped-file"), "{view}");
        assert!(!view.contains("secret/external.rs"));
    }

    #[test]
    fn provenance_completeness_is_per_declared_chain_for_shared_output() {
        let cancelled = AtomicBool::new(false);
        let mut store = provenance_source_store(None);
        let mut evidence = provenance_evidence();
        evidence.artifacts.push(imported(
            "failed-input",
            "failed.proto",
            ArtifactRole::Input,
            4,
        ));
        evidence.provenance.push(ProvenanceInput {
            input_key: "failed-input".into(),
            input_lines: EvidenceLineSpan { start: 1, end: 1 },
            generator_path: "src/missing.rs".into(),
            generator_lines: EvidenceLineSpan { start: 7, end: 7 },
            output_key: "output".into(),
            output_lines: EvidenceLineSpan { start: 1, end: 1 },
        });
        store
            .replace_evidence(generated_output_graph(), &evidence, &cancelled)
            .unwrap();

        let rendered = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(rendered.evidence.contains("provenance_model=partial"));
        assert!(rendered.evidence.contains(
            "claim kind=generated-provenance status=partial result=unknown basis=verified-generated-manifest input=\"failed.proto:1-1\" generator=\"src/missing.rs:7-7\" output=\"target/out.rs:1-1\""
        ));
        assert!(rendered.evidence.contains(
            "gap category=generated reason=generated-output-unobserved input=\"failed.proto:1-1\" generator=\"src/missing.rs:7-7\" output=\"target/out.rs:1-1\" occurrences=1"
        ));
        assert_eq!(
            rendered
                .evidence
                .matches("claim kind=generated-provenance")
                .count(),
            2,
            "{}",
            rendered.evidence
        );
        assert_eq!(rendered.dynamic_status, CompletenessStatus::Partial);
    }

    #[test]
    fn static_generated_gap_does_not_masquerade_as_failed_manifest_chain() {
        let cancelled = AtomicBool::new(false);
        let mut graph = single_node_graph("changed");
        graph.gaps.push(GapInput {
            file_key: Some("src/lib.rs".into()),
            source_key: Some("changed".into()),
            run_key: None,
            path: Some("src/lib.rs".into()),
            line_start: Some(1),
            line_end: Some(1),
            category: GapCategory::Generated,
            reason: GapReason::GeneratedOutputUnobserved,
            target_hint: Some("out.rs".into()),
            occurrences: 1,
            relation_site: false,
        });
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        store
            .replace_evidence(
                Graph::default(),
                &EvidenceInput {
                    artifacts: vec![imported(
                        "manifest",
                        "evidence.json",
                        ArtifactRole::Manifest,
                        1,
                    )],
                    ..EvidenceInput::default()
                },
                &cancelled,
            )
            .unwrap();

        let rendered = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(
            rendered
                .evidence
                .contains("provenance_model=not-applicable")
        );
        assert!(rendered.evidence.contains(
            "gap category=generated reason=generated-output-unobserved path=\"src/lib.rs\""
        ));
        assert!(
            !rendered
                .evidence
                .contains("claim kind=generated-provenance")
        );
    }

    #[test]
    fn generator_mapping_accepts_only_repository_rust_and_python_nodes() {
        let cancelled = AtomicBool::new(false);
        let cases = [
            ("src/generator.rs", Language::Rust, true),
            ("pkg/generator.py", Language::Python, true),
            ("web/generator.js", Language::JavaScript, false),
            ("web/generator.ts", Language::TypeScript, false),
        ];
        let graph = Graph {
            files: cases
                .iter()
                .enumerate()
                .map(|(index, (path, language, _))| FileInput {
                    path: (*path).into(),
                    language: *language,
                    git_oid: None,
                    content_hash: [u8::try_from(index + 1).unwrap(); 32],
                    parse_context: String::new(),
                    byte_size: 1,
                    replace: true,
                    observed_relation_sites: 0,
                })
                .collect(),
            nodes: cases
                .iter()
                .map(|(path, _, _)| NodeInput {
                    key: format!("generator:{path}"),
                    file_key: (*path).into(),
                    kind: NodeKind::Function,
                    name: "generate".into(),
                    qualified_name: format!("generate@{path}"),
                    parent_key: None,
                    owner_key: None,
                    line_start: 1,
                    line_end: 1,
                    signature: String::new(),
                    keys: Vec::new(),
                })
                .collect(),
            ..Graph::default()
        };
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        for (path, _, expected) in cases {
            assert_eq!(
                provenance_resolution(
                    &store.connection,
                    path,
                    EvidenceLineSpan { start: 1, end: 1 },
                    "out.rs",
                    false,
                )
                .unwrap()
                .generator_node_id
                .is_some(),
                expected,
                "unexpected generator mapping for {path}"
            );
        }
    }

    #[test]
    fn seal_recomputes_missing_and_ambiguous_aliases_without_conflating_them() {
        let cancelled = AtomicBool::new(false);
        let root = canonical_temp_dir().join(format!(
            "graphr-alias-state-seal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("graph.db");
        let node = |key: &str, keys: Vec<String>| NodeInput {
            key: key.into(),
            file_key: "app.py".into(),
            kind: NodeKind::Function,
            name: key.into(),
            qualified_name: key.into(),
            parent_key: None,
            owner_key: None,
            line_start: 1,
            line_end: 1,
            signature: String::new(),
            keys,
        };
        let graph = Graph {
            files: vec![FileInput {
                path: "app.py".into(),
                language: Language::Python,
                git_oid: None,
                content_hash: [1; 32],
                parse_context: "python".into(),
                byte_size: 1,
                replace: true,
                observed_relation_sites: 4,
            }],
            nodes: vec![
                node("exporter", Vec::new()),
                node("missing-consumer", Vec::new()),
                node("ambiguous-consumer", Vec::new()),
                node("candidate-one", vec!["candidate:multi".into()]),
                node("candidate-two", vec!["candidate:multi".into()]),
            ],
            refs: vec![
                RefInput {
                    source_key: "exporter".into(),
                    kind: RefKind::Imports,
                    line: 1,
                    keys: vec!["candidate:missing".into()],
                    alias_key: Some("alias:missing".into()),
                    resolved_target_key: None,
                    resolution: ResolutionState::Missing,
                },
                RefInput {
                    source_key: "exporter".into(),
                    kind: RefKind::Imports,
                    line: 1,
                    keys: vec!["candidate:multi".into()],
                    alias_key: Some("alias:multi".into()),
                    resolved_target_key: None,
                    resolution: ResolutionState::Ambiguous,
                },
                RefInput {
                    source_key: "missing-consumer".into(),
                    kind: RefKind::Calls,
                    line: 1,
                    keys: vec!["alias:missing".into()],
                    alias_key: None,
                    resolved_target_key: None,
                    resolution: ResolutionState::Missing,
                },
                RefInput {
                    source_key: "ambiguous-consumer".into(),
                    kind: RefKind::Calls,
                    line: 1,
                    keys: vec!["alias:multi".into()],
                    alias_key: None,
                    resolved_target_key: None,
                    resolution: ResolutionState::Ambiguous,
                },
            ],
            ..Graph::default()
        };
        let mut store = Store::open_private_image(&path, &cancelled).unwrap();
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();

        store.seal(&cancelled).unwrap();
        validate_image(&path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changes_and_view_emit_only_relevant_exact_static_and_dynamic_records() {
        let cancelled = AtomicBool::new(false);
        let source_file = |path: &str, byte| FileInput {
            path: path.into(),
            language: Language::Rust,
            git_oid: None,
            content_hash: [byte; 32],
            parse_context: "0:".into(),
            byte_size: 1,
            replace: true,
            observed_relation_sites: 1,
        };
        let source_node = |key: &str, path: &str| NodeInput {
            key: key.into(),
            file_key: path.into(),
            kind: NodeKind::Function,
            name: key.into(),
            qualified_name: key.into(),
            parent_key: None,
            owner_key: None,
            line_start: 1,
            line_end: 1,
            signature: String::new(),
            keys: Vec::new(),
        };
        let source = Graph {
            files: vec![
                source_file("src/changed.rs", 1),
                source_file("src/unrelated.rs", 2),
            ],
            nodes: vec![
                source_node("changed", "src/changed.rs"),
                source_node("unrelated", "src/unrelated.rs"),
            ],
            modeled_sites: vec![
                ModeledSiteInput {
                    file_key: "src/changed.rs".into(),
                    source_key: Some("changed".into()),
                    kind: ModeledSiteKind::GeneratedInclusion,
                    line_start: 1,
                    line_end: 1,
                    target_hint: Some("changed-out.rs".into()),
                    parse_context: Some("0:".into()),
                },
                ModeledSiteInput {
                    file_key: "src/unrelated.rs".into(),
                    source_key: Some("unrelated".into()),
                    kind: ModeledSiteKind::GeneratedInclusion,
                    line_start: 1,
                    line_end: 1,
                    target_hint: Some("unrelated-out.rs".into()),
                    parse_context: Some("0:".into()),
                },
            ],
            gaps: vec![
                GapInput {
                    file_key: Some("src/changed.rs".into()),
                    source_key: Some("changed".into()),
                    run_key: None,
                    path: Some("src/changed.rs".into()),
                    line_start: Some(1),
                    line_end: Some(1),
                    category: GapCategory::Relation,
                    reason: GapReason::DynamicOrUnsupportedDispatch,
                    target_hint: Some("changed-gap".into()),
                    occurrences: 1,
                    relation_site: false,
                },
                GapInput {
                    file_key: Some("src/unrelated.rs".into()),
                    source_key: Some("unrelated".into()),
                    run_key: None,
                    path: Some("src/unrelated.rs".into()),
                    line_start: Some(1),
                    line_end: Some(1),
                    category: GapCategory::Relation,
                    reason: GapReason::DynamicOrUnsupportedDispatch,
                    target_hint: Some("unrelated-gap".into()),
                    occurrences: 1,
                    relation_site: false,
                },
            ],
            ..Graph::default()
        };
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((source, ())))
            .unwrap();
        let evidence = EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 9),
                imported("changed-input", "changed.proto", ArtifactRole::Input, 3),
                imported(
                    "changed-output",
                    "target/changed-out.rs",
                    ArtifactRole::GeneratedRust,
                    4,
                ),
                imported("unrelated-input", "unrelated.proto", ArtifactRole::Input, 5),
                imported(
                    "unrelated-output",
                    "target/unrelated-out.rs",
                    ArtifactRole::GeneratedRust,
                    6,
                ),
                imported("report", "coverage.json", ArtifactRole::CoverageReport, 7),
            ],
            provenance: vec![
                ProvenanceInput {
                    input_key: "changed-input".into(),
                    input_lines: EvidenceLineSpan { start: 1, end: 1 },
                    generator_path: "src/changed.rs".into(),
                    generator_lines: EvidenceLineSpan { start: 1, end: 1 },
                    output_key: "changed-output".into(),
                    output_lines: EvidenceLineSpan { start: 1, end: 1 },
                },
                ProvenanceInput {
                    input_key: "unrelated-input".into(),
                    input_lines: EvidenceLineSpan { start: 1, end: 1 },
                    generator_path: "src/unrelated.rs".into(),
                    generator_lines: EvidenceLineSpan { start: 1, end: 1 },
                    output_key: "unrelated-output".into(),
                    output_lines: EvidenceLineSpan { start: 1, end: 1 },
                },
            ],
            runs: vec![CoverageRunInput {
                key: "run".into(),
                format: CoverageFormat::Llvm,
                report_key: "report".into(),
                run_label: "run".into(),
                test_name: None,
            }],
            regions: vec![
                coverage_region("run", "src/changed.rs", 1, 1, 1, None),
                coverage_region("run", "src/unrelated.rs", 1, 1, 1, None),
            ],
            ..EvidenceInput::default()
        };
        let generated = Graph {
            files: vec![
                FileInput {
                    path: "target/changed-out.rs".into(),
                    language: Language::Rust,
                    git_oid: None,
                    content_hash: [4; 32],
                    parse_context: "0:".into(),
                    byte_size: 1,
                    replace: true,
                    observed_relation_sites: 0,
                },
                FileInput {
                    path: "target/unrelated-out.rs".into(),
                    language: Language::Rust,
                    git_oid: None,
                    content_hash: [6; 32],
                    parse_context: "0:".into(),
                    byte_size: 1,
                    replace: true,
                    observed_relation_sites: 0,
                },
            ],
            ..Graph::default()
        };
        store
            .replace_evidence(generated, &evidence, &cancelled)
            .unwrap();

        let changes = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![ChangedFile {
                        path: "src/changed.rs".into(),
                        whole_file: true,
                        spans: Vec::new(),
                        report_unmapped: false,
                    }],
                    records: Vec::new(),
                    paths: Vec::new(),
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
        assert!(changes.graph.contains("gaps total=2"), "{}", changes.graph);
        assert!(changes.graph.contains("changed-gap"), "{}", changes.graph);
        assert!(
            !changes.graph.contains("unrelated-gap"),
            "{}",
            changes.graph
        );
        assert!(changes.evidence.contains("target/changed-out.rs"));
        assert!(changes.evidence.contains("path=\"src/changed.rs\""));
        assert!(!changes.evidence.contains("target/unrelated-out.rs"));
        assert!(!changes.evidence.contains("path=\"src/unrelated.rs\""));

        let state = read_state(&store.connection).unwrap();
        let node_id = store
            .connection
            .query_row("SELECT id FROM nodes WHERE name='changed'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        let unrelated_node_id = store
            .connection
            .query_row("SELECT id FROM nodes WHERE name='unrelated'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO edges(source_id,target_id,kind,support_count)
                 VALUES(?1,?2,'CALLS',1)",
                params![node_id, unrelated_node_id],
            )
            .unwrap();
        let view = store
            .view(
                SNAPSHOT,
                &format!(
                    "n1:{SNAPSHOT}:{}:{}:{node_id}",
                    state.epoch, state.generation
                ),
                1,
                10,
            )
            .unwrap();
        assert!(
            view.contains(" Function unrelated src/unrelated.rs:1"),
            "{view}"
        );
        assert!(view.contains("changed-gap"), "{view}");
        assert!(!view.contains("unrelated-gap"), "{view}");
        assert!(view.contains("target/changed-out.rs"), "{view}");
        assert!(!view.contains("target/unrelated-out.rs"), "{view}");
        assert!(view.contains("path=\"src/changed.rs\""), "{view}");
        assert!(
            !view.contains("claim kind=changed-execution path=\"src/unrelated.rs\""),
            "{view}"
        );
    }

    #[test]
    fn traverse_only_neighbor_does_not_enter_exact_evidence_scope() {
        let cancelled = AtomicBool::new(false);
        let file = |path: &str, byte, observed_relation_sites| FileInput {
            path: path.into(),
            language: Language::Rust,
            git_oid: None,
            content_hash: [byte; 32],
            parse_context: "0:".into(),
            byte_size: 1,
            replace: true,
            observed_relation_sites,
        };
        let node = |key: &str, path: &str| NodeInput {
            key: key.into(),
            file_key: path.into(),
            kind: NodeKind::Function,
            name: key.into(),
            qualified_name: key.into(),
            parent_key: None,
            owner_key: None,
            line_start: 1,
            line_end: 1,
            signature: String::new(),
            keys: Vec::new(),
        };
        let mut graph = Graph {
            files: vec![file("src/root.rs", 1, 0), file("src/neighbor.rs", 2, 1)],
            nodes: vec![
                node("root", "src/root.rs"),
                node("neighbor", "src/neighbor.rs"),
            ],
            edges: vec![EdgeInput {
                source_key: "root".into(),
                target_key: "neighbor".into(),
                kind: EdgeKind::Imports,
                support_count: 1,
            }],
            modeled_sites: vec![ModeledSiteInput {
                file_key: "src/neighbor.rs".into(),
                source_key: Some("neighbor".into()),
                kind: ModeledSiteKind::GeneratedInclusion,
                line_start: 1,
                line_end: 1,
                target_hint: Some("out.rs".into()),
                parse_context: Some("0:".into()),
            }],
            gaps: vec![GapInput {
                file_key: Some("src/neighbor.rs".into()),
                source_key: Some("neighbor".into()),
                run_key: None,
                path: Some("src/neighbor.rs".into()),
                line_start: Some(1),
                line_end: Some(1),
                category: GapCategory::Relation,
                reason: GapReason::DynamicOrUnsupportedDispatch,
                target_hint: Some("neighbor-gap".into()),
                occurrences: 1,
                relation_site: false,
            }],
            ..Graph::default()
        };
        own_graph_edges(&mut graph);
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        let evidence = EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 9),
                imported("input", "schema.proto", ArtifactRole::Input, 3),
                imported("output", "target/out.rs", ArtifactRole::GeneratedRust, 3),
                imported("report", "coverage.json", ArtifactRole::CoverageReport, 5),
            ],
            provenance: vec![ProvenanceInput {
                input_key: "input".into(),
                input_lines: EvidenceLineSpan { start: 1, end: 1 },
                generator_path: "src/neighbor.rs".into(),
                generator_lines: EvidenceLineSpan { start: 1, end: 1 },
                output_key: "output".into(),
                output_lines: EvidenceLineSpan { start: 1, end: 1 },
            }],
            runs: vec![CoverageRunInput {
                key: "run".into(),
                format: CoverageFormat::Llvm,
                report_key: "report".into(),
                run_label: "neighbor-run".into(),
                test_name: None,
            }],
            regions: vec![coverage_region("run", "src/neighbor.rs", 1, 1, 1, None)],
            ..EvidenceInput::default()
        };
        store
            .replace_evidence(generated_output_graph(), &evidence, &cancelled)
            .unwrap();

        let review = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: vec![ChangedFile {
                        path: "src/root.rs".into(),
                        whole_file: false,
                        spans: vec![LineSpan { start: 2, end: 2 }],
                        report_unmapped: false,
                    }],
                    records: Vec::new(),
                    paths: Vec::new(),
                    source_patch: String::new(),
                    artifacts: Default::default(),
                    skipped_paths: 0,
                },
                1,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(
            review.graph.contains("neighbor src/neighbor.rs:1"),
            "{}",
            review.graph
        );
        assert!(review.graph.contains("gaps total=1"), "{}", review.graph);
        assert!(!review.graph.contains("neighbor-gap"), "{}", review.graph);
        assert!(
            !review.evidence.contains("target/out.rs"),
            "{}",
            review.evidence
        );
        assert!(
            !review.evidence.contains("src/neighbor.rs"),
            "{}",
            review.evidence
        );
    }

    #[test]
    fn changed_input_span_pulls_its_exact_provenance_chain() {
        let cancelled = AtomicBool::new(false);
        let mut store = provenance_source_store(None);
        let mut evidence = provenance_evidence();
        evidence.artifacts.push(imported(
            "report",
            "coverage.json",
            ArtifactRole::CoverageReport,
            4,
        ));
        evidence.runs.push(CoverageRunInput {
            key: "run".into(),
            format: CoverageFormat::Llvm,
            report_key: "report".into(),
            run_label: "generated-run".into(),
            test_name: None,
        });
        evidence
            .regions
            .push(coverage_region("run", "target/out.rs", 1, 1, 1, None));
        let mut generated = generated_output_graph();
        generated.nodes.push(NodeInput {
            key: "generated".into(),
            file_key: "target/out.rs".into(),
            kind: NodeKind::Function,
            name: "generated".into(),
            qualified_name: "generated".into(),
            parent_key: None,
            owner_key: None,
            line_start: 1,
            line_end: 1,
            signature: String::new(),
            keys: Vec::new(),
        });
        store
            .replace_evidence(generated, &evidence, &cancelled)
            .unwrap();

        let review = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: Vec::new(),
                    records: Vec::new(),
                    paths: vec![crate::git::ChangedPath {
                        status: crate::git::ChangeStatus::Modified,
                        old_path: None,
                        old_language: None,
                        path: "schema.proto".into(),
                        language: None,
                        additions: Some(1),
                        deletions: Some(1),
                        layers: vec![crate::git::ChangeLayer::Unstaged],
                    }],
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
            review.evidence.contains(
                "claim kind=generated-provenance status=complete result=linked basis=verified-generated-manifest input=\"schema.proto:1-1\""
            ),
            "{}",
            review.evidence
        );
        assert!(review.evidence.contains("generator=\"src/lib.rs:1-1\""));
        assert!(review.evidence.contains("output=\"target/out.rs:1-1\""));
        assert!(
            review.evidence.contains(
                "claim kind=changed-execution path=\"target/out.rs\" lines=1 status=complete result=observed basis=llvm-coverage-json run=\"generated-run\""
            ),
            "{}",
            review.evidence
        );
    }

    #[test]
    fn provenance_expanded_range_keeps_an_owned_gap_relevant() {
        let cancelled = AtomicBool::new(false);
        let mut store = provenance_source_store(None);
        let mut evidence = provenance_evidence();
        evidence.provenance[0].output_lines = EvidenceLineSpan { start: 1, end: 5 };
        let mut generated = generated_output_graph();
        generated.nodes.push(NodeInput {
            key: "generated".into(),
            file_key: "target/out.rs".into(),
            kind: NodeKind::Function,
            name: "generated".into(),
            qualified_name: "generated".into(),
            parent_key: None,
            owner_key: None,
            line_start: 3,
            line_end: 3,
            signature: String::new(),
            keys: Vec::new(),
        });
        generated.gaps.push(GapInput {
            file_key: Some("target/out.rs".into()),
            source_key: Some("generated".into()),
            run_key: None,
            path: Some("target/out.rs".into()),
            line_start: Some(3),
            line_end: Some(3),
            category: GapCategory::Generated,
            reason: GapReason::GeneratedOutputUnobserved,
            target_hint: Some("nested.rs".into()),
            occurrences: 1,
            relation_site: false,
        });
        store
            .replace_evidence(generated, &evidence, &cancelled)
            .unwrap();

        let review = store
            .changes(
                SNAPSHOT,
                &WorktreeChanges {
                    files: Vec::new(),
                    records: Vec::new(),
                    paths: vec![crate::git::ChangedPath {
                        status: crate::git::ChangeStatus::Modified,
                        old_path: None,
                        old_language: None,
                        path: "schema.proto".into(),
                        language: None,
                        additions: Some(1),
                        deletions: Some(1),
                        layers: vec![crate::git::ChangeLayer::Unstaged],
                    }],
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
            review.evidence.contains(
                "gap category=generated reason=generated-output-unobserved path=\"target/out.rs\" line=3 target=\"nested.rs\" occurrences=1"
            ),
            "{}",
            review.evidence
        );
    }

    #[test]
    fn same_file_generated_gap_is_scoped_by_owner_not_path() {
        let cancelled = AtomicBool::new(false);
        let mut graph = single_node_graph("selected");
        graph.nodes[0].line_end = 2;
        graph.nodes.push(function_node("other", 10));
        graph.gaps.push(GapInput {
            file_key: Some("src/lib.rs".into()),
            source_key: Some("other".into()),
            run_key: None,
            path: Some("src/lib.rs".into()),
            line_start: Some(10),
            line_end: Some(10),
            category: GapCategory::Generated,
            reason: GapReason::GeneratedOutputUnobserved,
            target_hint: Some("other-generated.rs".into()),
            occurrences: 1,
            relation_site: false,
        });
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((graph, ())))
            .unwrap();
        store
            .replace_evidence(
                Graph::default(),
                &EvidenceInput {
                    artifacts: vec![imported(
                        "manifest",
                        "evidence.json",
                        ArtifactRole::Manifest,
                        1,
                    )],
                    ..EvidenceInput::default()
                },
                &cancelled,
            )
            .unwrap();

        let review = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                0,
                10,
                DependencyMode::Boundary,
                &cancelled,
            )
            .unwrap();
        assert!(review.graph.contains("gaps total=1"), "{}", review.graph);
        assert_eq!(review.static_status, CompletenessStatus::Partial);
        assert!(
            !review.evidence.contains("other-generated.rs"),
            "{}",
            review.evidence
        );

        let state = read_state(&store.connection).unwrap();
        let selected_id = store
            .connection
            .query_row("SELECT id FROM nodes WHERE name='selected'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        let view = store
            .view(
                SNAPSHOT,
                &format!(
                    "n1:{SNAPSHOT}:{}:{}:{selected_id}",
                    state.epoch, state.generation
                ),
                1,
                10,
            )
            .unwrap();
        assert!(!view.contains("other-generated.rs"), "{view}");
    }

    #[test]
    fn provenance_replacement_rejects_complete_link_without_generated_file() {
        let cancelled = AtomicBool::new(false);
        let mut store = provenance_source_store(None);

        let error = store
            .replace_evidence(Graph::default(), &provenance_evidence(), &cancelled)
            .unwrap_err();

        assert_eq!(error, "database provenance generated output is missing");
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM imported_artifacts", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn seal_and_image_validation_reject_complete_link_without_generated_file() {
        for validate_after_seal in [false, true] {
            let root = canonical_temp_dir().join(format!(
                "graphr-generated-provenance-invariant-{validate_after_seal}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            let path = root.join("graph.db");
            let mut store = provenance_source_store(Some(&path));
            store
                .replace_evidence(
                    generated_output_graph(),
                    &provenance_evidence(),
                    &AtomicBool::new(false),
                )
                .unwrap();
            store
                .connection
                .execute("DELETE FROM files WHERE path='target/out.rs'", [])
                .unwrap();

            let error = if validate_after_seal {
                store
                    .connection
                    .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
                    .unwrap();
                drop(store);
                validate_image(&path).unwrap_err()
            } else {
                store.seal(&AtomicBool::new(false)).unwrap_err()
            };

            assert_eq!(error, "database provenance generated output is missing");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rendering_marks_link_without_generated_file_partial() {
        let mut store = provenance_source_store(None);
        store
            .replace_evidence(
                generated_output_graph(),
                &provenance_evidence(),
                &AtomicBool::new(false),
            )
            .unwrap();
        store
            .connection
            .execute("DELETE FROM files WHERE path='target/out.rs'", [])
            .unwrap();

        let rendered = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                0,
                10,
                DependencyMode::Boundary,
                &AtomicBool::new(false),
            )
            .unwrap();

        assert!(rendered.evidence.contains(
            "claim kind=generated-provenance status=partial result=unknown basis=verified-generated-manifest"
        ));
        assert!(
            !rendered
                .evidence
                .contains("claim kind=generated-provenance status=complete result=linked")
        );
        assert_eq!(rendered.dynamic_status, CompletenessStatus::Partial);
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
        own_graph_edges(&mut graph);
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
            .unwrap()
            .graph;

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
                observed_relation_sites: 0,
            },
            FileInput {
                path: "src/other.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [2; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
                observed_relation_sites: 0,
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
        own_graph_edges(&mut graph);
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
            .unwrap()
            .graph;

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
                observed_relation_sites: 0,
            },
            FileInput {
                path: "src/other.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [2; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
                observed_relation_sites: 0,
            },
            FileInput {
                path: ".cargo/vendor/block-buffer/src/lib.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [3; 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
                observed_relation_sites: 0,
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
        own_graph_edges(&mut graph);
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
        own_graph_edges(&mut graph);
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

    #[test]
    fn dot_change_impact_exactly_renders_branched_merged_paths() {
        let flow_node = |id, kind: &str, name: &str, path: &str, line| FlowNode {
            id,
            kind: kind.into(),
            name: name.into(),
            qualified_name: name.into(),
            path: path.into(),
            line,
        };
        let left = flow_node(1, "test", "left_test", "tests/change.rs", 10);
        let right = flow_node(2, "function", "right", "src/lib.rs", 20);
        let merged = flow_node(3, "function", "merged", "src/lib.rs", 30);
        let target = flow_node(5, "function", "changed", "src/lib.rs", 50);
        let analysis = ChangeAnalysis {
            risks: HashMap::from([(
                5,
                NodeRisk {
                    score: 4_200,
                    flow_component: 4_200,
                    test_component: 0,
                    security_component: 0,
                    caller_component: 0,
                    test_node: false,
                    test_gap: false,
                    indirect_test_covered: false,
                },
            )]),
            flows: vec![
                AffectedFlow {
                    entry: left.clone(),
                    nodes: vec![left.clone(), merged.clone(), target.clone()],
                    parents: HashMap::from([(3, 1), (5, 3)]),
                    changed: vec![5],
                    depth: 2,
                    file_count: 2,
                    criticality: 4_200,
                },
                AffectedFlow {
                    entry: right.clone(),
                    nodes: vec![right, merged, target.clone()],
                    parents: HashMap::from([(3, 2), (5, 3)]),
                    changed: vec![5],
                    depth: 2,
                    file_count: 1,
                    criticality: 4_100,
                },
            ],
            flow_omitted: false,
            test_mapping_omitted: false,
        };
        let dot = change_dot(
            SNAPSHOT,
            &[RowNode {
                id: target.id,
                kind: target.kind,
                name: target.name,
                path: target.path,
                line: target.line,
            }],
            &analysis,
            &ChangeCalls {
                nodes: HashMap::new(),
                edges: BTreeSet::from([(1, 3, true), (2, 3, false), (3, 5, false)]),
            },
            (6, 50),
            DependencyMode::Boundary,
            DotAccounting {
                changed_total: 1,
                analysis_roots_omitted: 0,
                deleted_paths_unanalyzed: 0,
                unmapped_ranges: 0,
                file_mapped_ranges: 0,
                traversal_complete: true,
            },
        )
        .unwrap();

        assert_eq!(
            dot,
            format!(
                "digraph graphr_changes {{\n  graph [rankdir=LR, label=\"snapshot={SNAPSHOT} changed_emitted=1 changed_total=1 paths_emitted=2 paths_discovered=2 flow_discovery=complete render_complete=true analysis_roots_omitted=0 deleted_paths_unanalyzed=0 unmapped_ranges=0 file_mapped_ranges=0 traversal_complete=true\"];\n  n5 [style=filled, fillcolor=\"#fed7aa\", color=\"#c2410c\", penwidth=2, label=\"changed\\nsrc/lib.rs:50\\nchanged risk=0.4200\"];\n  n1 [style=filled, shape=ellipse, fillcolor=\"#dbeafe\", color=\"#2563eb\", label=\"left_test\\ntests/change.rs:10\"];\n  n3 [style=filled, label=\"merged\\nsrc/lib.rs:30\"];\n  n2 [style=filled, label=\"right\\nsrc/lib.rs:20\"];\n  n1 -> n3 [style=dashed];\n  n2 -> n3;\n  n3 -> n5;\n}}\n"
            )
        );
    }

    #[test]
    fn dot_change_impact_escapes_labels_and_preserves_framing() {
        assert_eq!(
            dot_escape("quote\" slash\\ line\nreturn\rtab\té"),
            "quote\\\" slash\\\\ line\\nreturn\\ntab\\té"
        );
        let long = "é".repeat(200);
        let shortened = shorten(&long, DOT_LABEL_PART_LIMIT);
        assert!(shortened.len() <= DOT_LABEL_PART_LIMIT);
        assert!(shortened.ends_with('…'));
    }

    #[test]
    fn dot_change_impact_is_one_bounded_document() {
        let (roots, analysis, calls, accounting) = oversized_dot_fixture(50);
        let dot = change_dot(
            SNAPSHOT,
            &roots,
            &analysis,
            &calls,
            (6, 50),
            DependencyMode::Boundary,
            accounting,
        )
        .unwrap();
        assert!(dot.len() <= DOT_BUDGET, "{}", dot.len());
        assert!(dot.starts_with("digraph graphr_changes {\n"));
        assert!(dot.ends_with("}\n"));
        assert!(dot.contains("render_complete=false"));
    }

    #[test]
    fn dot_change_impact_prunes_retained_context_before_changed_roots() {
        let (roots, analysis, calls, accounting) = oversized_dot_fixture(50);
        let without_context = change_dot(
            SNAPSHOT,
            &roots,
            &analysis,
            &ChangeCalls::default(),
            (6, 50),
            DependencyMode::Boundary,
            accounting,
        )
        .unwrap();
        let with_context = change_dot(
            SNAPSHOT,
            &roots,
            &analysis,
            &calls,
            (6, 50),
            DependencyMode::Boundary,
            accounting,
        )
        .unwrap();

        assert!(
            without_context.contains("changed_emitted=18"),
            "{without_context}"
        );
        assert!(
            with_context.contains("changed_emitted=18"),
            "{with_context}"
        );
    }

    #[test]
    fn dot_change_impact_prunes_low_priority_paths_before_direct_roots() {
        let (roots, analysis, accounting) = oversized_path_fixture(24);
        let dot = change_dot(
            SNAPSHOT,
            &roots,
            &analysis,
            &ChangeCalls::default(),
            (6, 50),
            DependencyMode::Boundary,
            accounting,
        )
        .unwrap();

        assert!(dot.len() <= DOT_BUDGET, "{}", dot.len());
        assert!(dot.contains("changed_emitted=1"));
        assert!(dot.contains("paths_discovered=24"));
        assert!(!dot.contains("paths_emitted=24"));
        assert!(dot.contains("changed_root"));
        assert!(dot.contains("caller_00_") && dot.contains("n2 -> n1;"));
        assert!(!dot.contains("caller_23_") && !dot.contains("n25 -> n1;"));
    }

    #[test]
    fn dot_change_impact_honors_zero_depth_and_one_node_boundaries() {
        let (roots, analysis, accounting) = oversized_path_fixture(1);
        let render = |depth, max_nodes| {
            change_dot(
                SNAPSHOT,
                &roots,
                &analysis,
                &ChangeCalls::default(),
                (depth, max_nodes),
                DependencyMode::Boundary,
                accounting,
            )
            .unwrap()
        };

        let depth_zero = render(0, 50);
        assert!(depth_zero.contains("changed_emitted=1"), "{depth_zero}");
        assert!(depth_zero.contains("  n1 ["), "{depth_zero}");
        assert!(!depth_zero.contains("  n2 [") && !depth_zero.contains(" -> "));

        let depth_one = render(1, 50);
        assert!(depth_one.contains("  n2 [") && depth_one.contains("n2 -> n1;"));

        let one_node = render(1, 1);
        assert!(one_node.contains("paths_emitted=0"), "{one_node}");
        assert!(one_node.contains("  n1 ["), "{one_node}");
        assert!(!one_node.contains("  n2 [") && !one_node.contains(" -> "));
    }

    #[test]
    fn dot_change_impact_marks_derived_and_dependency_nodes() {
        let root = RowNode {
            id: 1,
            kind: "type".into(),
            name: "changed_type".into(),
            path: "src/lib.rs".into(),
            line: 1,
        };
        let caller = FlowNode {
            id: 2,
            kind: "function".into(),
            name: "dependency".into(),
            qualified_name: "dependency".into(),
            path: ".cargo/vendor/example/src/lib.rs".into(),
            line: 2,
        };
        let affected = FlowNode {
            id: 3,
            kind: "function".into(),
            name: "affected".into(),
            qualified_name: "affected".into(),
            path: "src/lib.rs".into(),
            line: 3,
        };
        let analysis = ChangeAnalysis {
            risks: HashMap::from([(
                1,
                NodeRisk {
                    score: 1_000,
                    flow_component: 1_000,
                    test_component: 0,
                    security_component: 0,
                    caller_component: 0,
                    test_node: false,
                    test_gap: false,
                    indirect_test_covered: false,
                },
            )]),
            flows: vec![AffectedFlow {
                entry: caller.clone(),
                nodes: vec![caller, affected],
                parents: HashMap::from([(3, 2)]),
                changed: vec![3],
                depth: 1,
                file_count: 2,
                criticality: 1_000,
            }],
            flow_omitted: false,
            test_mapping_omitted: false,
        };
        let dot = change_dot(
            SNAPSHOT,
            &[root],
            &analysis,
            &ChangeCalls::default(),
            (6, 50),
            DependencyMode::Boundary,
            DotAccounting {
                changed_total: 1,
                analysis_roots_omitted: 0,
                deleted_paths_unanalyzed: 0,
                unmapped_ranges: 0,
                file_mapped_ranges: 0,
                traversal_complete: true,
            },
        )
        .unwrap();

        let affected_line = dot.lines().find(|line| line.starts_with("  n3 [")).unwrap();
        let dependency_line = dot.lines().find(|line| line.starts_with("  n2 [")).unwrap();
        assert!(affected_line.contains("affected") && affected_line.contains("#fef3c7"));
        assert!(dependency_line.contains("#e5e7eb"));
    }

    #[test]
    fn dot_change_impact_keeps_file_roots_as_analysis_only() {
        let affected = FlowNode {
            id: 2,
            kind: "function".into(),
            name: "affected".into(),
            qualified_name: "affected".into(),
            path: "src/lib.rs".into(),
            line: 2,
        };
        let analysis = ChangeAnalysis {
            flows: vec![AffectedFlow {
                entry: affected.clone(),
                nodes: vec![affected],
                parents: HashMap::new(),
                changed: vec![2],
                depth: 0,
                file_count: 1,
                criticality: 1_000,
            }],
            ..ChangeAnalysis::default()
        };
        let dot = change_dot(
            SNAPSHOT,
            &[RowNode {
                id: 1,
                kind: "file".into(),
                name: "src/lib.rs".into(),
                path: "src/lib.rs".into(),
                line: 1,
            }],
            &analysis,
            &ChangeCalls::default(),
            (6, 50),
            DependencyMode::Boundary,
            DotAccounting {
                changed_total: 0,
                analysis_roots_omitted: 0,
                deleted_paths_unanalyzed: 0,
                unmapped_ranges: 0,
                file_mapped_ranges: 1,
                traversal_complete: true,
            },
        )
        .unwrap();

        assert!(dot.contains("changed_emitted=0 changed_total=0"), "{dot}");
        assert!(dot.contains("file_mapped_ranges=1"), "{dot}");
        assert!(!dot.contains("  n1 ["), "{dot}");
        assert!(
            dot.lines().any(|line| {
                line.starts_with("  n2 [") && line.contains("affected") && line.contains("#fef3c7")
            }),
            "{dot}"
        );
        assert!(!dot.contains("changed risk="), "{dot}");
    }

    #[test]
    fn no_change_dot_is_valid_and_escaped() {
        let dot = no_change_dot(SNAPSHOT, "empty_\"worktree\\delta");
        assert!(dot.starts_with("digraph graphr_changes {\n"));
        assert!(dot.contains("no_changes_reason=empty_\\\"worktree\\\\delta"));
        assert!(!dot.contains("  n"));
        assert!(dot.ends_with("}\n"));
        assert!(dot.len() <= DOT_BUDGET);
    }

    fn oversized_dot_fixture(
        count: usize,
    ) -> (Vec<RowNode>, ChangeAnalysis, ChangeCalls, DotAccounting) {
        let mut roots = Vec::with_capacity(count);
        let mut risks = HashMap::with_capacity(count);
        let mut flows = Vec::with_capacity(count);
        let mut calls = ChangeCalls::default();
        for index in 0..count {
            let caller_id = i64::try_from(index * 2 + 1).unwrap();
            let target_id = caller_id + 1;
            let score = u32::try_from((count - index) * 100).unwrap();
            let target = RowNode {
                id: target_id,
                kind: "function".into(),
                name: format!("changed_{index}_{}", "x".repeat(320)),
                path: format!("src/{}/changed_{index}.rs", "p".repeat(320)),
                line: 2,
            };
            let caller = RowNode {
                id: caller_id,
                kind: "function".into(),
                name: format!("caller_{index}_{}", "y".repeat(320)),
                path: format!("src/{}/caller_{index}.rs", "q".repeat(320)),
                line: 1,
            };
            roots.push(target.clone());
            risks.insert(
                target_id,
                NodeRisk {
                    score,
                    flow_component: score,
                    test_component: 0,
                    security_component: 0,
                    caller_component: 0,
                    test_node: false,
                    test_gap: false,
                    indirect_test_covered: false,
                },
            );
            flows.push(AffectedFlow {
                entry: FlowNode {
                    id: caller.id,
                    kind: caller.kind.clone(),
                    name: caller.name.clone(),
                    qualified_name: caller.name.clone(),
                    path: caller.path.clone(),
                    line: caller.line,
                },
                nodes: vec![
                    FlowNode {
                        id: caller.id,
                        kind: caller.kind.clone(),
                        name: caller.name.clone(),
                        qualified_name: caller.name.clone(),
                        path: caller.path.clone(),
                        line: caller.line,
                    },
                    FlowNode {
                        id: target.id,
                        kind: target.kind.clone(),
                        name: target.name.clone(),
                        qualified_name: target.name.clone(),
                        path: target.path.clone(),
                        line: target.line,
                    },
                ],
                parents: HashMap::from([(target_id, caller_id)]),
                changed: vec![target_id],
                depth: 1,
                file_count: 2,
                criticality: score,
            });
            calls.nodes.insert(caller_id, caller);
            calls.nodes.insert(target_id, target);
            calls.edges.insert((caller_id, target_id, false));
        }
        (
            roots,
            ChangeAnalysis {
                risks,
                flows,
                flow_omitted: false,
                test_mapping_omitted: false,
            },
            calls,
            DotAccounting {
                changed_total: count,
                analysis_roots_omitted: 0,
                deleted_paths_unanalyzed: 0,
                unmapped_ranges: 0,
                file_mapped_ranges: 0,
                traversal_complete: true,
            },
        )
    }

    fn oversized_path_fixture(count: usize) -> (Vec<RowNode>, ChangeAnalysis, DotAccounting) {
        let root = RowNode {
            id: 1,
            kind: "function".into(),
            name: format!("changed_root_{}", "x".repeat(320)),
            path: format!("src/{}/changed.rs", "p".repeat(320)),
            line: 1,
        };
        let target = FlowNode {
            id: root.id,
            kind: root.kind.clone(),
            name: root.name.clone(),
            qualified_name: root.name.clone(),
            path: root.path.clone(),
            line: root.line,
        };
        let flows = (0..count)
            .map(|index| {
                let caller = FlowNode {
                    id: i64::try_from(index + 2).unwrap(),
                    kind: "function".into(),
                    name: format!("caller_{index:02}_{}", "y".repeat(320)),
                    qualified_name: format!("caller_{index:02}"),
                    path: format!("src/{}/caller_{index:02}.rs", "q".repeat(320)),
                    line: 2,
                };
                AffectedFlow {
                    entry: caller.clone(),
                    nodes: vec![caller.clone(), target.clone()],
                    parents: HashMap::from([(target.id, caller.id)]),
                    changed: vec![target.id],
                    depth: 1,
                    file_count: 2,
                    criticality: u32::try_from(count - index).unwrap(),
                }
            })
            .collect();
        (
            vec![root],
            ChangeAnalysis {
                risks: HashMap::from([(
                    target.id,
                    NodeRisk {
                        score: 1_000,
                        flow_component: 1_000,
                        test_component: 0,
                        security_component: 0,
                        caller_component: 0,
                        test_node: false,
                        test_gap: false,
                        indirect_test_covered: false,
                    },
                )]),
                flows,
                flow_omitted: false,
                test_mapping_omitted: false,
            },
            DotAccounting {
                changed_total: 1,
                analysis_roots_omitted: 0,
                deleted_paths_unanalyzed: 0,
                unmapped_ranges: 0,
                file_mapped_ranges: 0,
                traversal_complete: true,
            },
        )
    }

    #[test]
    fn completeness_direct_static_call_is_complete() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&AtomicBool::new(false), |_full, _existing| {
                Ok((resolution_graph(1), ()))
            })
            .unwrap();

        let review = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                6,
                50,
                DependencyMode::Boundary,
                &AtomicBool::new(false),
            )
            .unwrap();

        assert_eq!(review.static_status, CompletenessStatus::Complete);
        assert_eq!(review.dynamic_status, CompletenessStatus::NotApplicable);
        assert!(review.graph.contains(
            "completeness content_capture=complete source_capture=complete syntax_parse=complete site_classification=complete static_model=complete evidence_capture=not-applicable provenance_model=not-applicable execution_mapping=not-applicable traversal=complete"
        ));
        assert!(review.graph.contains("references missing=0 ambiguous=0"));
        for claim in ["affected-callers", "affected-flows", "static-test-paths"] {
            assert!(review.graph.contains(&format!(
                "claim kind={claim} status=complete basis=resolved-static-call-graph"
            )));
        }
    }

    #[test]
    fn completeness_traversal_can_finish_over_partial_static_evidence() {
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        let mut graph = single_node_graph("changed");
        graph.files[0].observed_relation_sites = 2;
        graph.gaps.extend([
            gap(GapCategory::Parse, GapReason::ParserError, false, 1),
            gap(
                GapCategory::Relation,
                GapReason::DynamicOrUnsupportedDispatch,
                true,
                2,
            ),
            gap(
                GapCategory::Macro,
                GapReason::MacroExpansionUnavailable,
                true,
                3,
            ),
        ]);
        store
            .index_with(&AtomicBool::new(false), |_full, _existing| Ok((graph, ())))
            .unwrap();

        let review = store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                6,
                50,
                DependencyMode::Boundary,
                &AtomicBool::new(false),
            )
            .unwrap();

        assert_eq!(review.static_status, CompletenessStatus::Partial);
        assert!(review.graph.contains("traversal_complete=true"));
        assert!(review.graph.contains(
            "completeness content_capture=complete source_capture=complete syntax_parse=partial site_classification=complete static_model=partial evidence_capture=not-applicable provenance_model=not-applicable execution_mapping=not-applicable traversal=complete"
        ));
        assert!(review.graph.contains(
            "gaps total=3 relevant=3 by_reason=parser-error:1,dynamic-or-unsupported-dispatch:1,macro-expansion-unavailable:1"
        ));
        assert!(
            review.graph.contains(
                "claim kind=affected-flows status=partial basis=resolved-static-call-graph"
            )
        );
        assert!(
            review
                .graph
                .contains("languages=rust,python,javascript,typescript")
        );
    }

    #[test]
    fn completeness_only_relevant_unresolved_references_are_partial() {
        let review = completeness_for_unresolved_reference("unrelated", false);
        assert_eq!(review.static_status, CompletenessStatus::Complete);
        assert!(review.graph.contains("references missing=1 ambiguous=0"));

        let review = completeness_for_unresolved_reference("changed-target", true);
        assert_eq!(review.static_status, CompletenessStatus::Partial);
        assert!(review.graph.contains("references missing=0 ambiguous=1"));
    }

    fn completeness_for_unresolved_reference(key: &str, ambiguous: bool) -> ChangeReview {
        let mut graph = single_node_graph("changed");
        graph.nodes[0].keys.push("changed-target".into());
        graph.nodes.push(NodeInput {
            keys: vec![if ambiguous {
                "changed-target".into()
            } else {
                "other-target".into()
            }],
            ..function_node("other", 2)
        });
        graph.nodes.push(function_node("source", 3));
        graph.files[0].observed_relation_sites = 1;
        graph.refs.push(RefInput {
            source_key: "source".into(),
            kind: RefKind::Calls,
            line: 3,
            keys: vec![key.into()],
            alias_key: None,
            resolved_target_key: None,
            resolution: if ambiguous {
                ResolutionState::Ambiguous
            } else {
                ResolutionState::Missing
            },
        });
        let mut store = Store {
            connection: Connection::open_in_memory().unwrap(),
        };
        store
            .index_with(&AtomicBool::new(false), |_full, _existing| Ok((graph, ())))
            .unwrap();
        store
            .changes(
                SNAPSHOT,
                &changed_lib(),
                6,
                50,
                DependencyMode::Boundary,
                &AtomicBool::new(false),
            )
            .unwrap()
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
                observed_relation_sites: 0,
            }],
            nodes: vec![function_node(name, 1)],
            ..Graph::default()
        }
    }

    fn own_graph_edges(graph: &mut Graph) {
        let edges = graph
            .edges
            .iter()
            .enumerate()
            .map(|(index, edge)| {
                (
                    index,
                    edge.source_key.clone(),
                    edge.target_key.clone(),
                    edge.kind,
                    edge.support_count,
                )
            })
            .collect::<Vec<_>>();
        for (index, source_key, target_key, kind, support_count) in edges {
            let source = graph
                .nodes
                .iter()
                .find(|node| node.key == source_key)
                .unwrap();
            let source_kind = source.kind;
            let source_file = source.file_key.clone();
            let line = source.line_start;
            match kind {
                EdgeKind::TestCalls => assert_eq!(source_kind, NodeKind::Test),
                EdgeKind::Calls => assert_ne!(source_kind, NodeKind::Test),
                EdgeKind::Imports => {}
            }
            let candidate = format!("test:owned-edge:{index}");
            graph
                .nodes
                .iter_mut()
                .find(|node| node.key == target_key)
                .unwrap()
                .keys
                .push(candidate.clone());
            for _ in 0..support_count {
                graph.refs.push(RefInput {
                    source_key: source_key.clone(),
                    kind: if kind == EdgeKind::Imports {
                        RefKind::Imports
                    } else {
                        RefKind::Calls
                    },
                    line,
                    keys: vec![candidate.clone()],
                    alias_key: None,
                    resolved_target_key: Some(target_key.clone()),
                    resolution: ResolutionState::Resolved,
                });
            }
            let file = graph
                .files
                .iter_mut()
                .find(|file| file.path == source_file)
                .unwrap();
            file.observed_relation_sites = file
                .observed_relation_sites
                .checked_add(support_count)
                .unwrap();
        }
    }

    fn sealed_resolution_corruption(label: &str, corrupt: impl FnOnce(&Connection)) -> String {
        let root = canonical_temp_dir().join(format!(
            "graphr-seal-{label}-{}-{}",
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
            .index_with(&cancelled, |_full, _existing| Ok((resolution_graph(1), ())))
            .unwrap();
        corrupt(&store.connection);

        let error = store.seal(&cancelled).unwrap_err();
        fs::remove_dir_all(root).unwrap();
        error
    }

    fn sealed_image_corruption(label: &str, corrupt: impl FnOnce(&Connection)) -> String {
        let root = canonical_temp_dir().join(format!(
            "graphr-image-{label}-{}-{}",
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
            .index_with(&cancelled, |_full, _existing| Ok((resolution_graph(1), ())))
            .unwrap();
        store.seal(&cancelled).unwrap();

        let connection = Connection::open(&path).unwrap();
        corrupt(&connection);
        connection.close().unwrap();
        let error = validate_image(&path).unwrap_err();
        fs::remove_dir_all(root).unwrap();
        error
    }

    fn changed_lib() -> WorktreeChanges {
        WorktreeChanges {
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
        }
    }

    fn gap(category: GapCategory, reason: GapReason, relation_site: bool, line: u32) -> GapInput {
        GapInput {
            file_key: Some("src/lib.rs".into()),
            source_key: Some("changed".into()),
            run_key: None,
            path: Some("src/lib.rs".into()),
            line_start: Some(line),
            line_end: Some(line),
            category,
            reason,
            target_hint: None,
            occurrences: 1,
            relation_site,
        }
    }

    fn resolution_graph(targets: usize) -> Graph {
        let mut graph = single_node_graph("source");
        graph.files[0].observed_relation_sites = 1;
        graph.nodes[0].keys.push("source".into());
        for index in 0..targets {
            let key = format!("target-{index}");
            graph.nodes.push(NodeInput {
                keys: vec!["callee".into()],
                ..function_node(&key, u32::try_from(index + 2).unwrap())
            });
        }
        let (resolution, resolved_target_key) = match targets {
            0 => (ResolutionState::Missing, None),
            1 => (ResolutionState::Resolved, Some("target-0".into())),
            _ => (ResolutionState::Ambiguous, None),
        };
        graph.refs.push(RefInput {
            source_key: "source".into(),
            kind: RefKind::Calls,
            line: 1,
            keys: vec!["callee".into()],
            alias_key: None,
            resolved_target_key: resolved_target_key.clone(),
            resolution,
        });
        if let Some(target_key) = resolved_target_key {
            graph.edges.push(EdgeInput {
                source_key: "source".into(),
                target_key,
                kind: EdgeKind::Calls,
                support_count: 1,
            });
        }
        graph
    }

    fn incremental_resolution_graph(targets: usize) -> Graph {
        let mut graph = Graph::default();
        graph.files.push(FileInput {
            path: "src/lib.rs".into(),
            language: Language::Rust,
            git_oid: None,
            content_hash: [0; 32],
            parse_context: String::new(),
            byte_size: 1,
            replace: false,
            observed_relation_sites: 1,
        });
        for index in 0..targets {
            let path = format!("src/target-{index}.rs");
            graph.files.push(FileInput {
                path: path.clone(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [u8::try_from(index + 1).unwrap(); 32],
                parse_context: String::new(),
                byte_size: 1,
                replace: true,
                observed_relation_sites: 0,
            });
            graph.nodes.push(NodeInput {
                key: format!("target-{index}"),
                file_key: path,
                kind: NodeKind::Function,
                name: format!("target-{index}"),
                qualified_name: format!("target-{index}"),
                parent_key: None,
                owner_key: None,
                line_start: 1,
                line_end: 1,
                signature: String::new(),
                keys: vec!["callee".into()],
            });
        }
        graph
    }

    fn imported(key: &str, path: &str, role: ArtifactRole, byte: u8) -> ImportedArtifactInput {
        ImportedArtifactInput {
            key: key.into(),
            path: path.into(),
            role,
            content_hash: [byte; 32],
            byte_size: 1,
        }
    }

    fn provenance_source_store(path: Option<&Path>) -> Store {
        let cancelled = AtomicBool::new(false);
        let mut source = single_node_graph("generator");
        source.modeled_sites.push(ModeledSiteInput {
            file_key: "src/lib.rs".into(),
            source_key: Some("generator".into()),
            kind: ModeledSiteKind::GeneratedInclusion,
            line_start: 1,
            line_end: 1,
            target_hint: Some("out.rs".into()),
            parse_context: Some("0:".into()),
        });
        source.files[0].observed_relation_sites = 1;
        let mut store = match path {
            Some(path) => Store::open_private_image(path, &cancelled).unwrap(),
            None => Store {
                connection: Connection::open_in_memory().unwrap(),
            },
        };
        store
            .index_with(&cancelled, |_full, _existing| Ok((source, ())))
            .unwrap();
        store
    }

    fn generated_output_graph() -> Graph {
        Graph {
            files: vec![FileInput {
                path: "target/out.rs".into(),
                language: Language::Rust,
                git_oid: None,
                content_hash: [3; 32],
                parse_context: "0:".into(),
                byte_size: 1,
                replace: true,
                observed_relation_sites: 0,
            }],
            ..Graph::default()
        }
    }

    fn provenance_evidence() -> EvidenceInput {
        EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                imported("input", "schema.proto", ArtifactRole::Input, 2),
                imported("output", "target/out.rs", ArtifactRole::GeneratedRust, 3),
            ],
            provenance: vec![ProvenanceInput {
                input_key: "input".into(),
                input_lines: EvidenceLineSpan { start: 1, end: 1 },
                generator_path: "src/lib.rs".into(),
                generator_lines: EvidenceLineSpan { start: 1, end: 1 },
                output_key: "output".into(),
                output_lines: EvidenceLineSpan { start: 1, end: 1 },
            }],
            ..EvidenceInput::default()
        }
    }

    fn global_gap_graph(reason: GapReason, occurrences: u32) -> Graph {
        Graph {
            gaps: vec![GapInput {
                file_key: None,
                source_key: None,
                run_key: None,
                path: Some("omitted.rs".into()),
                line_start: None,
                line_end: None,
                category: GapCategory::Source,
                reason,
                target_hint: None,
                occurrences,
                relation_site: false,
            }],
            ..Graph::default()
        }
    }

    fn stored_global_gaps(store: &Store) -> Vec<(String, i64)> {
        store
            .connection
            .prepare(
                "SELECT reason, occurrences FROM graph_gaps
                  WHERE file_id IS NULL AND source_id IS NULL AND run_id IS NULL
                  ORDER BY reason",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn assert_ref_state(store: &Store, state: &str, edges: i64) {
        assert_eq!(
            store
                .connection
                .query_row("SELECT resolution_state FROM refs", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            state
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM edges", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            edges
        );
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

    fn test_node(key: &str, name: &str, line: u32) -> NodeInput {
        NodeInput {
            key: key.into(),
            file_key: "src/lib.rs".into(),
            kind: NodeKind::Test,
            name: name.into(),
            qualified_name: key.into(),
            parent_key: None,
            owner_key: None,
            line_start: line,
            line_end: line,
            signature: String::new(),
            keys: vec![],
        }
    }

    fn coverage_region(
        run_key: &str,
        path: &str,
        start_line: u32,
        end_line: u32,
        execution_count: u64,
        context: Option<&str>,
    ) -> CoverageRegionInput {
        CoverageRegionInput {
            run_key: run_key.into(),
            path: Some(path.into()),
            start_line,
            start_column: 0,
            end_line,
            end_column: 0,
            execution_count,
            context: context.map(str::to_owned),
        }
    }

    fn coverage_mapping_evidence() -> EvidenceInput {
        EvidenceInput {
            artifacts: vec![
                imported("manifest", "evidence.json", ArtifactRole::Manifest, 1),
                imported("llvm-report", "llvm.json", ArtifactRole::CoverageReport, 2),
                imported(
                    "python-report",
                    "python.json",
                    ArtifactRole::CoverageReport,
                    3,
                ),
            ],
            provenance: Vec::new(),
            runs: vec![
                CoverageRunInput {
                    key: "llvm-run".into(),
                    format: CoverageFormat::Llvm,
                    report_key: "llvm-report".into(),
                    run_label: "rust".into(),
                    test_name: Some("named".into()),
                },
                CoverageRunInput {
                    key: "python-run".into(),
                    format: CoverageFormat::CoveragePy,
                    report_key: "python-report".into(),
                    run_label: "python".into(),
                    test_name: None,
                },
            ],
            regions: vec![
                coverage_region("llvm-run", "src/lib.rs", 1, 2, 1, None),
                coverage_region("llvm-run", "missing.rs", 1, 1, 1, None),
                coverage_region("llvm-run", "src/lib.rs", 20, 20, 0, None),
                coverage_region("python-run", "src/lib.rs", 1, 1, 1, Some("named")),
                coverage_region("python-run", "src/lib.rs", 2, 2, 1, Some("missing")),
                coverage_region("python-run", "src/lib.rs", 3, 3, 1, Some("ambiguous")),
            ],
            branches: vec![CoverageBranchInput {
                run_key: "python-run".into(),
                path: Some("src/lib.rs".into()),
                start_line: 1,
                start_column: 0,
                end_line: 1,
                end_column: 0,
                target_line: Some(2),
                kind: CoverageBranchKind::Arc,
                execution_count: 1,
            }],
            gaps: Vec::new(),
        }
    }
}
