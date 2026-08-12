use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::ops::Range;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;

use crate::git::{
    ArtifactReview, CapturedSource, ChangeStatus, ChangedPath, DependencyMode, Language,
    Repository, Source, SourceContent, SourceSnapshot, WorktreeChanges, changed_dependency_package,
    read_captured_source,
};
use crate::parse::{DefinitionKind, ParsedFile, RustParser};
use crate::python::PythonParser;
use crate::store::{
    EdgeInput, EdgeKind, FileInput, Graph, NodeInput, NodeKind, RefInput, RefKind, Store,
    TraitImplementationInput,
};
use crate::workspace::{
    BuildProgress, BuildStage, CACHE_FORMAT_VERSION, GRAPH_ANALYZER_VERSION, IndexCompletion,
    OperationError, Provenance, REVIEW_FORMAT_VERSION, ResolvedIndexRequest, SnapshotCatalog,
    SnapshotEntry, SnapshotKeyInput, SnapshotTarget, graph_image_key, selected_layers,
    snapshot_key, validate_entry_graph,
};

const QUALIFIED_PATH_LIMIT: usize = 1024;
const REVIEW_CONTEXT_BUDGET: usize = 8192;
const INITIAL_FILES_BUDGET: usize = 1792;
const INITIAL_DIFF_BUDGET: usize = 2432;
const INITIAL_ARTIFACTS_BUDGET: usize = 1920;
const INITIAL_GRAPH_BUDGET: usize = 1920;
const SECTION_OVERHEAD: usize = 704;
static LEGACY_CAPTURE_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct IndexStats {
    pub files_total: usize,
    pub files_reused: usize,
    pub files_parsed: usize,
    pub files_skipped: usize,
}

pub struct Engine {
    roots: Arc<crate::workspace::AllowedRoots>,
    catalog: Arc<SnapshotCatalog>,
    #[allow(dead_code)] // Task 6 caches bounded rendered review pages here.
    rendered: Mutex<HashMap<(String, u32, u32), Arc<ReviewSnapshot>>>,
}

impl Engine {
    pub fn new(roots: Arc<crate::workspace::AllowedRoots>) -> Self {
        Self {
            catalog: Arc::new(SnapshotCatalog::new(roots.clone())),
            roots,
            rendered: Mutex::new(HashMap::new()),
        }
    }

    pub fn roots(&self) -> &crate::workspace::AllowedRoots {
        &self.roots
    }

    pub fn snapshot(&self, snapshot_id: &str) -> Result<Arc<SnapshotEntry>, OperationError> {
        self.catalog.get(snapshot_id)
    }

    pub fn build_snapshot(
        &self,
        request: ResolvedIndexRequest,
        cancelled: &AtomicBool,
        progress: impl Fn(BuildProgress) + Sync,
    ) -> Result<IndexCompletion, OperationError> {
        let report =
            |stage, files_done, files_total, files_reused, files_parsed, rejected_cache| {
                progress(BuildProgress {
                    stage,
                    files_done,
                    files_total,
                    files_reused,
                    files_parsed,
                    rejected_cache,
                });
            };
        report(BuildStage::Capturing, 0, 0, 0, 0, None);
        let current = self.roots.inspect(&request.root.worktree_root, cancelled)?;
        if !same_workspace(&current, &request.root) {
            return Err(OperationError::new(
                crate::workspace::ErrorCode::RootStale,
                "resolved workspace identity changed before indexing",
            ));
        }
        self.catalog.attach(&current)?;
        let job = self.catalog.begin(&request.root)?;
        let repository = repository_from_identity(&request.root);
        let capture = repository.capture_snapshot(
            &request.base_oid,
            &request.head_oid,
            &request.target,
            request.dependency_mode,
            job.capture_root(),
            cancelled,
        )?;
        let total = capture.sources.files.len();
        report(BuildStage::Capturing, total, total, 0, 0, None);

        let graph_image_id = graph_image_key(
            &request.root.repository_id,
            &capture.sources.files,
            CACHE_FORMAT_VERSION,
            GRAPH_ANALYZER_VERSION,
            crate::store::SCHEMA_VERSION,
        );
        let review_bytes = rmcp::serde_json::to_vec(&capture.changes).map_err(|error| {
            OperationError::new(
                crate::workspace::ErrorCode::Internal,
                format!("cannot serialize captured review: {error}"),
            )
        })?;
        let review_id = blake3::hash(&review_bytes).to_hex().to_string();
        let snapshot_id = snapshot_key(
            &SnapshotKeyInput {
                graph_image_id: &graph_image_id,
                workspace_id: &request.root.workspace_id,
                base_oid: &request.base_oid,
                head_oid: &request.head_oid,
                target: &request.target,
                dependency_mode: request.dependency_mode,
                dirty_digest: &capture.dirty_digest,
                review_id: &review_id,
            },
            CACHE_FORMAT_VERSION,
            REVIEW_FORMAT_VERSION,
        );
        let provenance = Provenance {
            repository_id: request.root.repository_id.clone(),
            workspace_id: request.root.workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            common_git_dir: request.root.common_git_dir.clone(),
            git_dir: request.root.git_dir.clone(),
            repository_root: request.root.repository_root.clone(),
            worktree_root: request.root.worktree_root.clone(),
            branch: request.root.branch.clone(),
            base_ref: request.base_ref.clone(),
            base_oid: request.base_oid.clone(),
            head_ref: request.head_ref.clone(),
            head_oid: request.head_oid.clone(),
            target_state: request.target.clone(),
            selected_layers: selected_layers(&capture.changes),
            dirty_digest: capture.dirty_digest.clone(),
            commits_base_to_head: capture.commits_base_to_head,
            changed_files: capture.changed_files,
            index_generation: 0,
        };

        let mut rejected_cache =
            self.catalog
                .prepare_publication(&job, &snapshot_id, &review_id, &review_bytes)?;
        if let Some(path) = self
            .catalog
            .quarantine_rejected(&request.root, &snapshot_id)?
        {
            rejected_cache = Some(path);
        }
        report(
            BuildStage::SelectingSeed,
            0,
            total,
            0,
            0,
            rejected_cache.clone(),
        );
        let exact_graph = job.graph_path(&graph_image_id);
        let mut candidates = self.catalog.entries(&request.root.repository_id);
        let trusted_exact = candidates
            .iter()
            .find(|entry| entry.graph_image_id == graph_image_id)
            .cloned();
        let exact = match fs::symlink_metadata(&exact_graph) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(OperationError::new(
                    crate::workspace::ErrorCode::CacheCorrupt,
                    format!("cannot inspect exact graph cache: {error}"),
                ));
            }
            Ok(_) => match trusted_exact.as_deref().map_or_else(
                || crate::workspace::validate_published_image(&exact_graph),
                validate_entry_graph,
            ) {
                Ok(_) => true,
                Err(_) => {
                    rejected_cache = Some(exact_graph.display().to_string());
                    self.catalog
                        .quarantine_graph(&request.root, &graph_image_id, &snapshot_id)?;
                    report(
                        BuildStage::SelectingSeed,
                        0,
                        total,
                        0,
                        0,
                        rejected_cache.clone(),
                    );
                    false
                }
            },
        };

        let (stats, graph_temp) = if exact {
            let stats = IndexStats {
                files_total: total,
                files_reused: total,
                files_parsed: 0,
                files_skipped: capture.sources.skipped,
            };
            report(
                BuildStage::Indexing,
                total,
                total,
                total,
                0,
                rejected_cache.clone(),
            );
            (stats, None)
        } else {
            candidates.retain(|entry| entry.graph_image_id != graph_image_id);
            candidates.sort_by(|left, right| {
                let left_base = left.provenance.head_oid == request.base_oid;
                let right_base = right.provenance.head_oid == request.base_oid;
                right_base
                    .cmp(&left_base)
                    .then_with(|| {
                        right
                            .provenance
                            .index_generation
                            .cmp(&left.provenance.index_generation)
                    })
                    .then_with(|| {
                        left.provenance
                            .snapshot_id
                            .cmp(&right.provenance.snapshot_id)
                    })
            });
            for candidate in candidates {
                match validate_entry_graph(&candidate) {
                    Ok(_) => {
                        job.copy_seed(&candidate.graph_path)?;
                        break;
                    }
                    Err(_) => {
                        rejected_cache = Some(candidate.graph_path.display().to_string());
                        self.catalog.quarantine_graph(
                            &request.root,
                            &candidate.graph_image_id,
                            &candidate.provenance.snapshot_id,
                        )?;
                        report(
                            BuildStage::SelectingSeed,
                            0,
                            total,
                            0,
                            0,
                            rejected_cache.clone(),
                        );
                    }
                }
            }
            report(BuildStage::Indexing, 0, total, 0, 0, rejected_cache.clone());
            let mut store =
                Store::open(job.graph_temp(), false, cancelled).map_err(index_operation_error)?;
            let (_, _, stats) = store
                .index_with(cancelled, |full, existing| {
                    build_index(
                        &repository,
                        &capture.sources,
                        cancelled,
                        full,
                        existing,
                        |done, total, reused| {
                            report(
                                BuildStage::Indexing,
                                done,
                                total,
                                reused,
                                done.saturating_sub(reused),
                                rejected_cache.clone(),
                            );
                        },
                    )
                })
                .map_err(index_operation_error)?;
            report(
                BuildStage::Indexing,
                total,
                total,
                stats.files_reused,
                stats.files_parsed,
                rejected_cache.clone(),
            );
            report(
                BuildStage::ResolvingGraph,
                total,
                total,
                stats.files_reused,
                stats.files_parsed,
                rejected_cache.clone(),
            );
            store.seal(cancelled).map_err(index_operation_error)?;
            crate::store::validate_image(job.graph_temp()).map_err(|error| {
                OperationError::new(
                    crate::workspace::ErrorCode::CacheCorrupt,
                    format!("private graph image is invalid: {error}"),
                )
            })?;
            (stats, Some(job.graph_temp()))
        };
        if exact {
            report(
                BuildStage::ResolvingGraph,
                total,
                total,
                stats.files_reused,
                stats.files_parsed,
                rejected_cache.clone(),
            );
        }
        check_cancelled(cancelled).map_err(index_operation_error)?;
        report(
            BuildStage::Publishing,
            total,
            total,
            stats.files_reused,
            stats.files_parsed,
            rejected_cache,
        );
        let entry = self.catalog.publish(
            &job,
            &graph_image_id,
            &review_id,
            &review_bytes,
            graph_temp,
            request.dependency_mode,
            capture.no_change_reason,
            provenance,
        )?;
        Ok(IndexCompletion {
            snapshot_id,
            graph_image_id,
            provenance: entry.provenance.clone(),
            stats,
        })
    }
}

fn same_workspace(
    current: &crate::workspace::RootIdentity,
    resolved: &crate::workspace::RootIdentity,
) -> bool {
    current.repository_id == resolved.repository_id
        && current.workspace_id == resolved.workspace_id
        && current.repository_root == resolved.repository_root
        && current.worktree_root == resolved.worktree_root
        && current.git_dir == resolved.git_dir
        && current.common_git_dir == resolved.common_git_dir
        && current.index_path == resolved.index_path
        && current.object_format == resolved.object_format
}

fn repository_from_identity(root: &crate::workspace::RootIdentity) -> Repository {
    Repository {
        root: root.worktree_root.clone(),
        git_dir: root.git_dir.clone(),
        common_git_dir: root.common_git_dir.clone(),
        index_path: root.index_path.clone(),
        branch: root.branch.clone(),
        head_oid: root.head_oid.clone(),
        object_format: root.object_format.clone(),
    }
}

fn index_operation_error(error: String) -> OperationError {
    let code = if error.contains("cancelled") {
        crate::workspace::ErrorCode::JobCancelled
    } else {
        crate::workspace::ErrorCode::Internal
    };
    OperationError::new(code, error)
}

#[derive(Clone)]
pub struct Project {
    repository: Arc<Repository>,
    review_snapshot: Arc<Mutex<Option<ReviewSnapshot>>>,
}

impl Project {
    pub fn open(path: &Path) -> Result<Self, String> {
        Self::open_cancelled(path, &AtomicBool::new(false))
    }

    pub fn open_cancelled(path: &Path, cancelled: &AtomicBool) -> Result<Self, String> {
        Ok(Self {
            repository: Arc::new(
                Repository::discover_for_project_cancelled(path, cancelled)
                    .map_err(|error| error.to_string())?,
            ),
            review_snapshot: Arc::new(Mutex::new(None)),
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
        let database = self.repository.git_dir.join("graphr/index.db");
        let mut store = Store::open(&database, rebuild, &cancelled)?;
        let capture = LegacyCapture::create(&self.repository.git_dir.join("graphr"))?;
        let target = SnapshotTarget::Worktree {
            include_untracked: true,
        };
        let sources = if self.repository.head_oid.is_empty() {
            self.repository
                .capture_sources(
                    &self.repository.head_oid,
                    &target,
                    &capture.path,
                    &cancelled,
                )
                .map_err(|error| error.to_string())?
        } else {
            self.repository
                .capture_snapshot(
                    &self.repository.head_oid,
                    &self.repository.head_oid,
                    &target,
                    DependencyMode::Boundary,
                    &capture.path,
                    &cancelled,
                )
                .map_err(|error| error.to_string())?
                .sources
        };
        let (state, changed, stats) = store.index_with(&cancelled, |full, existing| {
            build_index(
                &self.repository,
                &sources,
                &cancelled,
                full,
                existing,
                |_, _, _| {},
            )
        })?;
        Ok(format!(
            "indexed generation={} changed={} skipped={}",
            state.generation, changed, stats.files_skipped
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
        Store::open_reader(&self.repository.git_dir.join("graphr/index.db"))?
            .search(query, kind, limit)
    }

    pub fn view(&self, node_ref: &str, depth: u32, max_nodes: u32) -> Result<String, String> {
        Store::open_reader(&self.repository.git_dir.join("graphr/index.db"))?
            .view(node_ref, depth, max_nodes)
    }

    pub fn changes_cancelled(
        &self,
        base: &str,
        depth: u32,
        max_nodes: u32,
        dependency_mode: DependencyMode,
        cursor: Option<&str>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<String, String> {
        check_cancelled(&cancelled)?;
        if let Some(cursor) = cursor {
            let cursor = parse_review_cursor(cursor)?;
            let snapshot = self
                .review_snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let snapshot = snapshot
                .as_ref()
                .filter(|snapshot| snapshot.matches(base, depth, max_nodes, dependency_mode))
                .ok_or_else(|| "stale changes cursor".to_owned())?;
            return render_section(snapshot, &cursor);
        }

        let changes = self
            .repository
            .worktree_changes(base, dependency_mode, &cancelled)?;
        if changes.is_empty() {
            *self
                .review_snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
            return Ok("no changes\n".into());
        }
        let graph = Store::open_reader(&self.repository.git_dir.join("graphr/index.db"))?.changes(
            &changes,
            depth,
            max_nodes,
            dependency_mode,
            &cancelled,
        )?;
        let snapshot = ReviewSnapshot::new(base, depth, max_nodes, dependency_mode, changes, graph);
        let output = review_context(&snapshot)?;
        // ponytail: retain one bounded review snapshot; use a keyed LRU only if
        // concurrent independent review paginations become a real requirement.
        *self
            .review_snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(snapshot);
        Ok(output)
    }
}

struct LegacyCapture {
    path: PathBuf,
}

impl LegacyCapture {
    fn create(parent: &Path) -> Result<Self, String> {
        let path = parent.join(format!(
            ".capture-{}-{}",
            std::process::id(),
            LEGACY_CAPTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("cannot create source capture: {error}"))?;
        Ok(Self {
            path: fs::canonicalize(path)
                .map_err(|error| format!("cannot resolve source capture: {error}"))?,
        })
    }
}

impl Drop for LegacyCapture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Default)]
struct DependencyPackageSummary {
    files: usize,
    source_files: usize,
    checksum_files: usize,
    statuses: [usize; 7],
}

fn change_manifest(changes: &WorktreeChanges, dependency_mode: DependencyMode) -> String {
    let mut output = String::new();
    let mut dependencies = BTreeMap::<&str, DependencyPackageSummary>::new();
    let mut dependency_files = 0_usize;
    let mut dependency_hash = blake3::Hasher::new();
    for path in &changes.paths {
        if dependency_mode == DependencyMode::Boundary
            && let Some(package) = changed_dependency_package(path)
        {
            let mut record = String::new();
            change_path_line(&mut record, path, &changes.artifacts);
            dependency_hash.update(&(record.len() as u64).to_le_bytes());
            dependency_hash.update(record.as_bytes());
            dependency_files += 1;
            let summary = dependencies.entry(package).or_default();
            summary.files += 1;
            summary.source_files += usize::from(path.language.is_some());
            summary.checksum_files += usize::from(path.path.ends_with("/.cargo-checksum.json"));
            summary.statuses[change_status_index(path.status)] += 1;
            continue;
        }
        change_path_line(&mut output, path, &changes.artifacts);
    }
    if !dependencies.is_empty() {
        output.push_str(&format!(
            "dependency-boundary root=.cargo/vendor packages={} files={} path_digest={}\n",
            dependencies.len(),
            dependency_files,
            dependency_hash.finalize().to_hex(),
        ));
        for (package, summary) in dependencies {
            output.push_str(&format!(
                "dependency-package name={package} files={} source_files={} checksum_files={} added={} modified={} deleted={} renamed={} type_changed={} unmerged={} untracked={}\n",
                summary.files,
                summary.source_files,
                summary.checksum_files,
                summary.statuses[0],
                summary.statuses[1],
                summary.statuses[2],
                summary.statuses[3],
                summary.statuses[4],
                summary.statuses[5],
                summary.statuses[6],
            ));
        }
    }
    if changes.skipped_paths > 0 {
        output.push_str("skipped ");
        output.push_str(&changes.skipped_paths.to_string());
        output.push_str(" unsafe paths\n");
    }
    output
}

const fn change_status_index(status: ChangeStatus) -> usize {
    match status {
        ChangeStatus::Added => 0,
        ChangeStatus::Modified => 1,
        ChangeStatus::Deleted => 2,
        ChangeStatus::Renamed => 3,
        ChangeStatus::TypeChanged => 4,
        ChangeStatus::Unmerged => 5,
        ChangeStatus::Untracked => 6,
    }
}

fn change_path_line(output: &mut String, path: &ChangedPath, artifacts: &ArtifactReview) {
    match path.status {
        ChangeStatus::Added => output.push_str("added "),
        ChangeStatus::Modified => output.push_str("changed "),
        ChangeStatus::Deleted => output.push_str("deleted "),
        ChangeStatus::Renamed => output.push_str("renamed "),
        ChangeStatus::TypeChanged => output.push_str("type-changed "),
        ChangeStatus::Unmerged => output.push_str("unmerged "),
        ChangeStatus::Untracked => output.push_str("untracked "),
    }
    if let Some(language) = path.language {
        output.push_str("source ");
        output.push_str(language.as_str());
        output.push(' ');
    } else if let Some(file) = artifacts.file(&path.path) {
        output.push_str("artifact ");
        output.push_str(if file.diff_complete {
            "text "
        } else {
            "omitted "
        });
    } else {
        output.push_str("artifact omitted ");
    }
    if let Some(old) = &path.old_path {
        output.push_str(old);
        output.push_str(" -> ");
    }
    output.push_str(&path.path);
    if path.language.is_none()
        && let Some(file) = artifacts.file(&path.path)
    {
        output.push_str(" analyzer=");
        output.push_str(file.analyzer.as_str());
        if !file.analysis_complete && file.diff_complete {
            output.push_str(" analysis=omitted");
        }
        if let Some(reason) = file.omission {
            output.push_str(" reason=");
            output.push_str(reason.as_str());
        }
    }
    if path.status == ChangeStatus::Modified && path.language.is_some() {
        output.push_str(" status=modified");
    }
    match (path.additions, path.deletions) {
        (Some(additions), Some(deletions)) => {
            output.push_str(&format!(" additions={additions} deletions={deletions}"));
        }
        _ => output.push_str(" additions=unknown deletions=unknown"),
    }
    output.push_str(" layers=");
    for (index, layer) in path.layers.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(layer.as_str());
    }
    output.push('\n');
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewSection {
    Files,
    Diff,
    Artifacts,
    Graph,
}

impl ReviewSection {
    const fn code(self) -> char {
        match self {
            Self::Files => 'f',
            Self::Diff => 'd',
            Self::Artifacts => 'a',
            Self::Graph => 'g',
        }
    }

    const fn header(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Diff => "diff",
            Self::Artifacts => "artifacts",
            Self::Graph => "graph",
        }
    }

    const fn cursor_label(self) -> &'static str {
        match self {
            Self::Files => "files_next_cursor",
            Self::Diff => "diff_next_cursor",
            Self::Artifacts => "artifacts_next_cursor",
            Self::Graph => "graph_next_cursor",
        }
    }
}

struct ReviewCursor {
    section: ReviewSection,
    offset: usize,
    checksum: String,
}

struct ReviewSnapshot {
    base: String,
    depth: u32,
    max_nodes: u32,
    dependency_mode: DependencyMode,
    manifest: String,
    artifacts: String,
    changes: Arc<WorktreeChanges>,
    graph: String,
    checksum: String,
    file_ranges: Vec<Range<usize>>,
    hunk_ranges: Vec<Range<usize>>,
    artifact_file_ranges: Vec<Range<usize>>,
    artifact_record_ranges: Vec<Range<usize>>,
    artifact_hunk_ranges: Vec<Range<usize>>,
    graph_record_ranges: Vec<Range<usize>>,
    flow_ranges: Vec<Range<usize>>,
    patch_totals: String,
    artifact_patch_totals: String,
    all_path_totals: String,
    all_path_hunks: String,
    complete_after_pagination: bool,
}

impl ReviewSnapshot {
    fn new(
        base: &str,
        depth: u32,
        max_nodes: u32,
        dependency_mode: DependencyMode,
        changes: impl Into<Arc<WorktreeChanges>>,
        graph: String,
    ) -> Self {
        let changes = changes.into();
        let manifest = change_manifest(&changes, dependency_mode);
        let artifacts = artifact_text(&changes.artifacts);
        let checksum = review_snapshot(
            base,
            depth,
            max_nodes,
            dependency_mode,
            [&manifest, &changes.source_patch, &artifacts, &graph],
        );
        let file_ranges = line_ranges(&manifest, None);
        let artifact_file_ranges = line_ranges(&artifacts, Some("artifact "));
        let mut artifact_record_ranges = line_ranges(&artifacts, Some("markdown "));
        artifact_record_ranges.extend(line_ranges(&artifacts, Some("tsv ")));
        artifact_record_ranges.sort_unstable_by_key(|range| range.start);
        let artifact_hunk_ranges = hunk_ranges(&artifacts);
        let hunk_ranges = hunk_ranges(&changes.source_patch);
        let graph_record_ranges = line_ranges(&graph, None);
        let flow_ranges = line_ranges(&graph, Some("flow "));
        let all_path_hunks = change_hunk_totals(
            &changes,
            hunk_ranges.len(),
            artifact_hunk_ranges.len(),
            dependency_mode,
        );
        let patch_totals = change_totals(
            "patch",
            changes
                .paths
                .iter()
                .filter(|path| path_in_source_patch(path, dependency_mode)),
            0,
        );
        let artifact_patch_totals = change_totals(
            "patch",
            changes
                .paths
                .iter()
                .filter(|path| path_in_artifact_patch(path, &changes, dependency_mode)),
            0,
        );
        let all_path_totals =
            change_totals("all_path", changes.paths.iter(), changes.skipped_paths);
        let complete_after_pagination =
            graph_review_complete(&graph) && change_content_complete(&changes, dependency_mode);
        Self {
            base: base.into(),
            depth,
            max_nodes,
            dependency_mode,
            manifest,
            artifacts,
            changes,
            graph,
            checksum,
            file_ranges,
            hunk_ranges,
            artifact_file_ranges,
            artifact_record_ranges,
            artifact_hunk_ranges,
            graph_record_ranges,
            flow_ranges,
            patch_totals,
            artifact_patch_totals,
            all_path_totals,
            all_path_hunks,
            complete_after_pagination,
        }
    }

    fn matches(
        &self,
        base: &str,
        depth: u32,
        max_nodes: u32,
        dependency_mode: DependencyMode,
    ) -> bool {
        self.base == base
            && self.depth == depth
            && self.max_nodes == max_nodes
            && self.dependency_mode == dependency_mode
    }

    fn value(&self, section: ReviewSection) -> &str {
        match section {
            ReviewSection::Files => &self.manifest,
            ReviewSection::Diff => &self.changes.source_patch,
            ReviewSection::Artifacts => &self.artifacts,
            ReviewSection::Graph => &self.graph,
        }
    }

    fn ranges(&self, section: ReviewSection) -> &[Range<usize>] {
        match section {
            ReviewSection::Files => &self.file_ranges,
            ReviewSection::Diff => &self.hunk_ranges,
            ReviewSection::Artifacts => &self.artifact_hunk_ranges,
            ReviewSection::Graph => &self.flow_ranges,
        }
    }
}

fn artifact_text(review: &ArtifactReview) -> String {
    let mut output = String::new();
    for file in &review.files {
        output.push_str(&format!(
            "artifact path={:?} analyzer={} diff_complete={} analysis_complete={}",
            file.path,
            file.analyzer.as_str(),
            file.diff_complete,
            file.analysis_complete,
        ));
        if let Some(reason) = file.omission {
            output.push_str(" reason=");
            output.push_str(reason.as_str());
        }
        output.push('\n');
    }
    output.push_str(&review.analysis);
    if !review.analysis.is_empty() && !review.patch.is_empty() && !review.analysis.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&review.patch);
    output
}

struct Page<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn review_context(snapshot: &ReviewSnapshot) -> Result<String, String> {
    let (files, files_more) =
        render_section_page(snapshot, ReviewSection::Files, 0, INITIAL_FILES_BUDGET)?;
    let (diff, diff_more) =
        render_section_page(snapshot, ReviewSection::Diff, 0, INITIAL_DIFF_BUDGET)?;
    let (artifacts, artifacts_more) = render_section_page(
        snapshot,
        ReviewSection::Artifacts,
        0,
        INITIAL_ARTIFACTS_BUDGET,
    )?;
    let (graph_page, graph_more) =
        render_section_page(snapshot, ReviewSection::Graph, 0, INITIAL_GRAPH_BUDGET)?;
    let output = format!(
        "{files}{diff}{artifacts}{graph_page}review_complete={} review_complete_when_pages_exhausted={}\n",
        !files_more
            && !diff_more
            && !artifacts_more
            && !graph_more
            && snapshot.complete_after_pagination,
        snapshot.complete_after_pagination,
    );
    if output.len() > REVIEW_CONTEXT_BUDGET {
        return Err("review context exceeds output budget".into());
    }
    Ok(output)
}

fn render_section(snapshot: &ReviewSnapshot, cursor: &ReviewCursor) -> Result<String, String> {
    let expected = cursor_checksum(&snapshot.checksum, cursor.section, cursor.offset);
    if cursor.checksum != expected {
        return Err("stale changes cursor".into());
    }
    let completion = format!(
        "review_complete_when_pages_exhausted={}\n",
        snapshot.complete_after_pagination
    );
    let page_budget = REVIEW_CONTEXT_BUDGET
        .checked_sub(completion.len())
        .ok_or_else(|| "review completion metadata exceeds output budget".to_owned())?;
    let (mut output, _) =
        render_section_page(snapshot, cursor.section, cursor.offset, page_budget)?;
    output.push_str(&completion);
    if output.len() > REVIEW_CONTEXT_BUDGET {
        return Err("review section exceeds output budget".into());
    }
    Ok(output)
}

fn render_section_page(
    snapshot: &ReviewSnapshot,
    section: ReviewSection,
    offset: usize,
    budget: usize,
) -> Result<(String, bool), String> {
    let value = snapshot.value(section);
    let content_budget = budget
        .checked_sub(SECTION_OVERHEAD)
        .ok_or_else(|| "review section budget is too small".to_owned())?;
    let page = page(value, offset, content_budget)?;
    let page = if section == ReviewSection::Graph {
        limit_page_records(
            value,
            page,
            &snapshot.graph_record_ranges,
            snapshot.max_nodes as usize,
        )
    } else {
        page
    };
    let more = page.end < value.len();
    let emitted_bytes = page.end - page.start;
    let starts_mid_line = page.start > 0 && value.as_bytes()[page.start - 1] != b'\n';
    let ends_mid_line =
        page.end < value.len() && page.end > 0 && value.as_bytes()[page.end - 1] != b'\n';
    let framing_suffix_bytes = usize::from(!page.text.is_empty() && !page.text.ends_with('\n'));
    let mut output = format!("{}\n", section.header());
    match section {
        ReviewSection::Files => {
            let coverage = record_coverage(snapshot.ranges(section), &page);
            output.push_str(&format!(
                "files dependency_mode={} rename_detection=within-source-and-artifact emitted_bytes={} total_bytes={} byte_range={}..{} starts_mid_line={} ends_mid_line={} framing_suffix_bytes={} emitted_entries={} partial_entries={} total_entries={} prior_entries={} remaining_entries={} page_complete={}\n",
                snapshot.dependency_mode.as_str(),
                emitted_bytes,
                value.len(),
                page.start,
                page.end,
                starts_mid_line,
                ends_mid_line,
                framing_suffix_bytes,
                coverage.emitted,
                coverage.partial,
                coverage.total,
                coverage.prior,
                coverage.remaining,
                !more
            ));
        }
        ReviewSection::Diff => {
            let coverage = record_coverage(snapshot.ranges(section), &page);
            output.push_str(&format!(
                "diff scope=source dependency_mode={} emitted_bytes={} total_bytes={} prior_bytes={} remaining_bytes={} byte_range={}..{} starts_mid_line={} ends_mid_line={} framing_suffix_bytes={} emitted_hunks={} partial_hunks={} total_hunks={} prior_hunks={} remaining_hunks={} {} {} {} page_complete={}\n",
                snapshot.dependency_mode.as_str(),
                emitted_bytes,
                value.len(),
                page.start,
                value.len() - page.end,
                page.start,
                page.end,
                starts_mid_line,
                ends_mid_line,
                framing_suffix_bytes,
                coverage.emitted,
                coverage.partial,
                coverage.total,
                coverage.prior,
                coverage.remaining,
                snapshot.patch_totals,
                snapshot.all_path_totals,
                snapshot.all_path_hunks,
                !more
            ));
        }
        ReviewSection::Artifacts => {
            let files = record_coverage(&snapshot.artifact_file_ranges, &page);
            let records = record_coverage(&snapshot.artifact_record_ranges, &page);
            let hunks = record_coverage(&snapshot.artifact_hunk_ranges, &page);
            output.push_str(&format!(
                "artifacts emitted_bytes={} total_bytes={} prior_bytes={} remaining_bytes={} byte_range={}..{} starts_mid_line={} ends_mid_line={} framing_suffix_bytes={} emitted_files={} partial_files={} total_files={} prior_files={} remaining_files={} emitted_records={} partial_records={} total_records={} prior_records={} remaining_records={} emitted_hunks={} partial_hunks={} total_hunks={} prior_hunks={} remaining_hunks={} {} analysis_complete={} page_complete={}\n",
                emitted_bytes,
                value.len(),
                page.start,
                value.len() - page.end,
                page.start,
                page.end,
                starts_mid_line,
                ends_mid_line,
                framing_suffix_bytes,
                files.emitted,
                files.partial,
                files.total,
                files.prior,
                files.remaining,
                records.emitted,
                records.partial,
                records.total,
                records.prior,
                records.remaining,
                hunks.emitted,
                hunks.partial,
                hunks.total,
                hunks.prior,
                hunks.remaining,
                snapshot.artifact_patch_totals,
                snapshot.changes.artifacts.analysis_complete(),
                !more,
            ));
        }
        ReviewSection::Graph => {
            let coverage = record_coverage(snapshot.ranges(section), &page);
            let records = record_coverage(&snapshot.graph_record_ranges, &page);
            let analysis_complete = graph_flow_analysis_complete(&snapshot.graph);
            let neighborhood_complete =
                graph_summary_value(&snapshot.graph, "neighborhood_omitted") == Some("false");
            let mapping_complete =
                graph_summary_value(&snapshot.graph, "unmapped_ranges") == Some("0");
            let flow_total = if analysis_complete {
                coverage.total.to_string()
            } else {
                "unknown".into()
            };
            output.push_str(&format!(
                "graph emitted_bytes={} total_bytes={} prior_bytes={} remaining_bytes={} byte_range={}..{} starts_mid_line={} ends_mid_line={} framing_suffix_bytes={} page_record_limit={} emitted_records={} partial_records={} total_records={} prior_records={} remaining_records={} emitted_flows={} partial_flows={} discovered_flows={} total_flows={} prior_flows={} remaining_discovered_flows={} page_complete={} analysis_complete={} neighborhood_complete={} mapping_complete={}\n",
                emitted_bytes,
                value.len(),
                page.start,
                value.len() - page.end,
                page.start,
                page.end,
                starts_mid_line,
                ends_mid_line,
                framing_suffix_bytes,
                snapshot.max_nodes,
                records.emitted,
                records.partial,
                records.total,
                records.prior,
                records.remaining,
                coverage.emitted,
                coverage.partial,
                coverage.total,
                flow_total,
                coverage.prior,
                coverage.remaining,
                !more,
                analysis_complete,
                neighborhood_complete,
                mapping_complete,
            ));
        }
    }
    output.push_str(page.text);
    if !page.text.is_empty() && !page.text.ends_with('\n') {
        output.push('\n');
    }
    if more {
        output.push_str(section.cursor_label());
        output.push('=');
        output.push_str(&cursor_token(&snapshot.checksum, section, page.end));
        output.push('\n');
    }
    if output.len() > budget {
        return Err("review section exceeds output budget".into());
    }
    Ok((output, more))
}

fn page(value: &str, offset: usize, budget: usize) -> Result<Page<'_>, String> {
    if value.is_empty() && offset == 0 {
        return Ok(Page {
            start: 0,
            end: 0,
            text: "",
        });
    }
    if offset >= value.len() || !value.is_char_boundary(offset) {
        return Err("invalid changes cursor".into());
    }
    let mut end = value.len().min(offset.saturating_add(budget));
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    if end < value.len()
        && let Some(newline) = value[offset..end].rfind('\n')
    {
        end = offset + newline + 1;
    }
    if end == offset {
        return Err("review section cannot make progress".into());
    }
    Ok(Page {
        start: offset,
        end,
        text: &value[offset..end],
    })
}

fn limit_page_records<'a>(
    value: &'a str,
    page: Page<'a>,
    ranges: &[Range<usize>],
    limit: usize,
) -> Page<'a> {
    let first = ranges.partition_point(|range| range.end <= page.start);
    let Some(last) = first
        .checked_add(limit)
        .and_then(|end| ranges.get(end.saturating_sub(1)))
    else {
        return page;
    };
    let end = page.end.min(last.end);
    Page {
        start: page.start,
        end,
        text: &value[page.start..end],
    }
}

fn review_snapshot(
    base: &str,
    depth: u32,
    max_nodes: u32,
    dependency_mode: DependencyMode,
    sections: [&str; 4],
) -> String {
    let depth = depth.to_string();
    let max_nodes = max_nodes.to_string();
    let dependency_mode = dependency_mode.as_str();
    let mut hash = blake3::Hasher::new();
    for value in [
        b"graphr changes v1".as_slice(),
        base.as_bytes(),
        depth.as_bytes(),
        max_nodes.as_bytes(),
        dependency_mode.as_bytes(),
    ]
    .into_iter()
    .chain(sections.into_iter().map(str::as_bytes))
    {
        hash.update(&(value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    hash.finalize().to_hex().to_string()
}

fn cursor_checksum(snapshot: &str, section: ReviewSection, offset: usize) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(snapshot.as_bytes());
    hash.update(&[section.code() as u8]);
    hash.update(&offset.to_le_bytes());
    hash.finalize().to_hex().to_string()
}

fn cursor_token(snapshot: &str, section: ReviewSection, offset: usize) -> String {
    format!(
        "v1:{}:{offset}:{}",
        section.code(),
        cursor_checksum(snapshot, section, offset)
    )
}

fn parse_review_cursor(value: &str) -> Result<ReviewCursor, String> {
    let mut parts = value.split(':');
    let version = parts.next();
    let section = match parts.next() {
        Some("f") => ReviewSection::Files,
        Some("d") => ReviewSection::Diff,
        Some("a") => ReviewSection::Artifacts,
        Some("g") => ReviewSection::Graph,
        _ => return Err("invalid changes cursor".into()),
    };
    let offset = parts
        .next()
        .filter(|offset| !offset.is_empty() && offset.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|offset| offset.parse().ok())
        .ok_or_else(|| "invalid changes cursor".to_owned())?;
    let checksum = parts
        .next()
        .filter(|checksum| {
            checksum.len() == 64
                && checksum
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
        .ok_or_else(|| "invalid changes cursor".to_owned())?;
    if version != Some("v1") || parts.next().is_some() {
        return Err("invalid changes cursor".into());
    }
    Ok(ReviewCursor {
        section,
        offset,
        checksum: checksum.into(),
    })
}

fn change_totals<'a>(
    scope: &str,
    paths: impl Iterator<Item = &'a ChangedPath>,
    unknown: usize,
) -> String {
    let (additions, deletions, unknown) = paths.fold(
        (0_u64, 0_u64, unknown),
        |(additions, deletions, unknown), path| match (path.additions, path.deletions) {
            (Some(added), Some(deleted)) => (
                additions.saturating_add(added),
                deletions.saturating_add(deleted),
                unknown,
            ),
            _ => (additions, deletions, unknown + 1),
        },
    );
    if unknown == 0 {
        format!("{scope}_additions={additions} {scope}_deletions={deletions}")
    } else {
        format!(
            "{scope}_additions_at_least={additions} {scope}_deletions_at_least={deletions} {scope}_unknown_stats={unknown}"
        )
    }
}

fn path_in_source_patch(path: &ChangedPath, dependency_mode: DependencyMode) -> bool {
    (path.language.is_some() || path.old_language.is_some())
        && (dependency_mode == DependencyMode::Full || changed_dependency_package(path).is_none())
        && path.additions.is_some()
        && path.deletions.is_some()
}

fn path_in_artifact_patch(
    path: &ChangedPath,
    changes: &WorktreeChanges,
    dependency_mode: DependencyMode,
) -> bool {
    path.language.is_none()
        && (dependency_mode == DependencyMode::Full || changed_dependency_package(path).is_none())
        && path.additions.is_some()
        && path.deletions.is_some()
        && changes
            .artifacts
            .file(&path.path)
            .is_some_and(|file| file.diff_complete)
}

fn change_hunk_totals(
    changes: &WorktreeChanges,
    source_hunks: usize,
    artifact_hunks: usize,
    dependency_mode: DependencyMode,
) -> String {
    let mut total = source_hunks.saturating_add(artifact_hunks);
    let mut unknown = changes.skipped_paths;
    for path in &changes.paths {
        let captured = path_in_source_patch(path, dependency_mode)
            || path_in_artifact_patch(path, changes, dependency_mode);
        if captured {
            continue;
        }
        if path.status == ChangeStatus::Untracked {
            if let Some(additions) = path.additions {
                total = total.saturating_add(usize::from(additions > 0));
            } else {
                unknown = unknown.saturating_add(1);
            }
        } else {
            unknown = unknown.saturating_add(1);
        }
    }
    if unknown == 0 {
        format!("all_path_hunks={total}")
    } else {
        format!("all_path_hunks_at_least={total} all_path_unknown_hunk_paths={unknown}")
    }
}

struct RecordCoverage {
    total: usize,
    prior: usize,
    emitted: usize,
    partial: usize,
    remaining: usize,
}

fn record_coverage(ranges: &[Range<usize>], page: &Page<'_>) -> RecordCoverage {
    let prior = ranges.partition_point(|range| range.end <= page.start);
    let remaining_start = ranges.partition_point(|range| range.start < page.end);
    let mut coverage = RecordCoverage {
        total: ranges.len(),
        prior,
        emitted: 0,
        partial: 0,
        remaining: ranges.len() - remaining_start,
    };
    for range in &ranges[prior..remaining_start] {
        if range.start >= page.start && range.end <= page.end {
            coverage.emitted += 1;
        } else {
            coverage.partial += 1;
        }
    }
    coverage
}

fn line_ranges(value: &str, prefix: Option<&str>) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    for line in value.split_inclusive('\n') {
        let end = offset + line.len();
        if prefix.is_none_or(|prefix| line.starts_with(prefix)) {
            ranges.push(offset..end);
        }
        offset = end;
    }
    ranges
}

fn hunk_ranges(value: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut current = None;
    let mut offset = 0;
    for line in value.split_inclusive('\n') {
        if line.starts_with("diff --git ") || line.starts_with("@@ ") {
            if let Some(start) = current.take() {
                ranges.push(start..offset);
            }
            if line.starts_with("@@ ") {
                current = Some(offset);
            }
        }
        offset += line.len();
    }
    if let Some(start) = current {
        ranges.push(start..value.len());
    }
    ranges
}

fn graph_flow_analysis_complete(value: &str) -> bool {
    graph_summary_value(value, "analysis_complete") == Some("true")
}

fn graph_summary_value<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    value
        .lines()
        .next()?
        .split_ascii_whitespace()
        .find_map(|field| {
            let (key, value) = field.split_once('=')?;
            (key == name).then_some(value)
        })
}

fn graph_review_complete(value: &str) -> bool {
    graph_flow_analysis_complete(value)
        && graph_summary_value(value, "changed_symbols_omitted") == Some("0")
        && graph_summary_value(value, "neighborhood_omitted") == Some("false")
        && graph_summary_value(value, "unmapped_ranges") == Some("0")
}

fn change_content_complete(changes: &WorktreeChanges, dependency_mode: DependencyMode) -> bool {
    changes.skipped_paths == 0
        && changes.artifacts.is_complete()
        && changes
            .artifacts
            .files
            .iter()
            .all(|file| file.omission.is_none())
        && changes.paths.iter().all(|path| {
            if dependency_mode == DependencyMode::Boundary
                && changed_dependency_package(path).is_some()
            {
                true
            } else if path.language.is_none() {
                changes.artifacts.file(&path.path).is_some_and(|file| {
                    file.diff_complete && file.analysis_complete && file.omission.is_none()
                })
            } else {
                (path.status != ChangeStatus::Renamed || path.old_language.is_some())
                    && path.additions.is_some()
                    && path.deletions.is_some()
            }
        })
}

fn build_index(
    repository: &Repository,
    sources: &SourceSnapshot,
    cancelled: &AtomicBool,
    full: bool,
    existing: &HashMap<String, crate::store::StoredFile>,
    progress: impl Fn(usize, usize, usize) + Sync,
) -> Result<(Graph, IndexStats), String> {
    let total = sources.files.len();
    let mut stats = IndexStats {
        files_total: total,
        files_reused: 0,
        files_parsed: 0,
        files_skipped: sources.skipped,
    };
    progress(0, total, 0);
    let mut outputs = (0..total).map(|_| None).collect::<Vec<Option<Graph>>>();
    let mut pending = Vec::new();
    for (index, file) in sources.files.iter().enumerate() {
        check_cancelled(cancelled)?;
        let rust_target = (file.language == Language::Rust)
            .then(|| TargetPath::from_parse_context(&file.parse_context))
            .transpose()?;
        let old = existing.get(&file.path);
        if !full
            && old.is_some_and(|old| {
                old.language == file.language
                    && old.parse_context == file.parse_context
                    && old.git_oid == file.git_oid
                    && stored_content_key(old) == file.content_key
            })
        {
            let old = old.expect("checked above");
            let mut graph = Graph::default();
            graph.files.push(FileInput {
                path: file.path.clone(),
                language: file.language,
                git_oid: file.git_oid.clone(),
                content_hash: old.content_hash,
                parse_context: file.parse_context.clone(),
                byte_size: old.byte_size,
                replace: false,
            });
            outputs[index] = Some(graph);
            stats.files_reused += 1;
            progress(stats.files_reused, total, stats.files_reused);
            continue;
        }
        pending.push(FileWork {
            index,
            file,
            rust_target,
        });
    }

    // ponytail: avoid thread startup below five files; revisit for a few very large files.
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(pending.len().div_ceil(4))
        .max(1);
    if workers == 1 {
        let mut rust_parser = None;
        let mut python_parser = None;
        let mut blob_reader = None;
        let mut completed = stats.files_reused;
        for work in &pending {
            match build_file(
                repository,
                sources,
                cancelled,
                work,
                &mut rust_parser,
                &mut python_parser,
                &mut blob_reader,
            )? {
                Some(graph) => {
                    outputs[work.index] = Some(graph);
                    stats.files_parsed += 1;
                }
                None => stats.files_skipped += 1,
            }
            completed += 1;
            progress(completed, total, stats.files_reused);
        }
    } else {
        let next = AtomicUsize::new(0);
        let completed = AtomicUsize::new(stats.files_reused);
        let progress_lock = Mutex::new(());
        let parts = thread::scope(|scope| {
            let handles = (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut rust_parser = None;
                        let mut python_parser = None;
                        let mut blob_reader = None;
                        let mut parts = Vec::new();
                        while let Some(work) = pending.get(next.fetch_add(1, Ordering::Relaxed)) {
                            let part = build_file(
                                repository,
                                sources,
                                cancelled,
                                work,
                                &mut rust_parser,
                                &mut python_parser,
                                &mut blob_reader,
                            )?;
                            parts.push((work.index, part));
                            let _guard = progress_lock
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            progress(
                                completed.fetch_add(1, Ordering::Relaxed) + 1,
                                total,
                                stats.files_reused,
                            );
                        }
                        Ok::<_, String>(parts)
                    })
                })
                .collect::<Vec<_>>();
            let mut parts = Vec::with_capacity(pending.len());
            for handle in handles {
                parts.extend(
                    handle
                        .join()
                        .map_err(|_| "source parser worker panicked".to_owned())??,
                );
            }
            Ok::<_, String>(parts)
        })?;
        for (index, part) in parts {
            match part {
                Some(graph) => {
                    outputs[index] = Some(graph);
                    stats.files_parsed += 1;
                }
                None => stats.files_skipped += 1,
            }
        }
    }

    let mut graph = Graph::default();
    for mut part in outputs.into_iter().flatten() {
        graph.files.append(&mut part.files);
        graph.nodes.append(&mut part.nodes);
        graph.refs.append(&mut part.refs);
        graph
            .trait_implementations
            .append(&mut part.trait_implementations);
        graph.edges.append(&mut part.edges);
    }
    if full {
        resolve(&mut graph, cancelled)?;
    }
    Ok((graph, stats))
}

fn stored_content_key(file: &crate::store::StoredFile) -> String {
    file.git_oid.clone().unwrap_or_else(|| {
        blake3::Hash::from_bytes(file.content_hash)
            .to_hex()
            .to_string()
    })
}

struct FileWork<'a> {
    index: usize,
    file: &'a CapturedSource,
    rust_target: Option<TargetPath>,
}

fn build_file(
    repository: &Repository,
    sources: &SourceSnapshot,
    cancelled: &AtomicBool,
    work: &FileWork<'_>,
    rust_parser: &mut Option<RustParser>,
    python_parser: &mut Option<PythonParser>,
    blob_reader: &mut Option<crate::git::BlobReader>,
) -> Result<Option<Graph>, String> {
    let content = match &work.file.content {
        SourceContent::GitBlob(oid) => {
            if work.file.git_oid.as_deref() != Some(oid) || work.file.content_key != *oid {
                return Err("captured Git source identity mismatch".into());
            }
            if blob_reader.is_none() {
                *blob_reader = Some(repository.blob_reader()?);
            }
            let content = blob_reader
                .as_mut()
                .expect("initialized above")
                .read(oid, cancelled)?;
            let Some(content) = content else {
                *blob_reader = None;
                return Ok(None);
            };
            content
        }
        SourceContent::Captured {
            relative_path,
            digest,
        } => read_captured_source(&sources.capture_root, relative_path, digest, cancelled)?,
    };
    let Ok(text) = String::from_utf8(content) else {
        return Ok(None);
    };
    let source = Source {
        path: work.file.path.clone(),
        text,
    };
    let content_hash = *blake3::hash(source.text.as_bytes()).as_bytes();
    let byte_size = u64::try_from(source.text.len())
        .map_err(|_| "source byte size exceeds supported range".to_owned())?;
    let mut graph = Graph::default();
    graph.files.push(FileInput {
        path: work.file.path.clone(),
        language: work.file.language,
        git_oid: work.file.git_oid.clone(),
        content_hash,
        parse_context: work.file.parse_context.clone(),
        byte_size,
        replace: true,
    });
    match work.file.language {
        Language::Rust => {
            if rust_parser.is_none() {
                *rust_parser = Some(RustParser::new()?);
            }
            add_rust_file(
                &mut graph,
                &source,
                work.rust_target.as_ref().expect("checked above"),
                rust_parser.as_mut().expect("initialized above"),
            )?;
        }
        Language::Python => {
            if python_parser.is_none() {
                *python_parser = Some(PythonParser::new()?);
            }
            crate::python::add_file(
                &mut graph,
                &source,
                python_parser.as_mut().expect("initialized above"),
            )?;
        }
    }
    Ok(Some(graph))
}

#[cfg(test)]
pub(crate) fn build_snapshot_for_test(
    repository: &Repository,
    sources: &SourceSnapshot,
    cancelled: &AtomicBool,
) -> Result<Graph, String> {
    build_index(
        repository,
        sources,
        cancelled,
        true,
        &HashMap::new(),
        |_, _, _| {},
    )
    .map(|(graph, _)| graph)
}

#[cfg(test)]
fn build_graph(sources: &[Source], cancelled: &AtomicBool) -> Result<Graph, String> {
    let mut parser = RustParser::new()?;
    let mut targets = TargetLayout::from_sources(sources);
    let mut graph = Graph {
        files: Vec::with_capacity(sources.len()),
        nodes: Vec::new(),
        refs: Vec::new(),
        trait_implementations: Vec::new(),
        edges: Vec::new(),
    };
    for source in sources {
        check_cancelled(cancelled)?;
        let target = targets.for_path(&source.path);
        graph.files.push(FileInput {
            path: source.path.clone(),
            language: Language::Rust,
            git_oid: None,
            content_hash: *blake3::hash(source.text.as_bytes()).as_bytes(),
            parse_context: target.parse_context(),
            byte_size: u64::try_from(source.text.len())
                .map_err(|_| "source byte size exceeds supported range".to_owned())?,
            replace: true,
        });
        add_rust_file(&mut graph, source, &target, &mut parser)?;
    }
    resolve(&mut graph, cancelled)?;
    Ok(graph)
}

fn add_rust_file(
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
        let owner_key = (definition.impl_target.is_some() && definition.parent.is_none())
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

    let mut values = HashMap::<usize, HashMap<String, Binding>>::new();
    for binding in &parsed.bindings {
        values
            .entry(binding.source)
            .or_default()
            .entry(binding.name.clone())
            .and_modify(|value| *value = Binding::Ambiguous)
            .or_insert_with(|| {
                binding
                    .type_target
                    .clone()
                    .map_or(Binding::Ambiguous, Binding::Unique)
            });
    }
    let bindings = Bindings { imports, values };
    for implementation in &parsed.implementations {
        let implementation_module = lexical_module(implementation.module, module, &module_paths);
        let Some(implementor) = normalize_impl_target(
            &implementation.type_target,
            implementation.module,
            implementation_module,
            &target.root,
            &bindings.imports,
        ) else {
            continue;
        };
        let Some(trait_) = normalize_impl_target(
            &implementation.trait_target,
            implementation.module,
            implementation_module,
            &target.root,
            &bindings.imports,
        ) else {
            continue;
        };
        graph.trait_implementations.push(TraitImplementationInput {
            file_key: source.path.clone(),
            line_start: to_u32(implementation.line_start)?,
            line_end: to_u32(implementation.line_end)?,
            implementor_key: item_key(&implementor),
            trait_key: item_key(&trait_),
        });
    }
    for import in &parsed.imports {
        let import_module = lexical_module(import.module, module, &module_paths);
        let Some(path) = normalize_use(&import.path, import_module, &target.root) else {
            continue;
        };
        let alias_key = import
            .exported
            .then(|| use_binding(&import.path, import_module, &target.root))
            .flatten()
            .map(|(alias, _)| item_key(&join_path(import_module, &alias)));
        graph.refs.push(RefInput {
            source_key: import
                .source
                .and_then(|source| node_keys.get(source).cloned())
                .unwrap_or_else(|| file_key.clone()),
            kind: RefKind::Imports,
            line: to_u32(import.line)?,
            keys: vec![item_key(&path)],
            alias_key,
            resolved_target_key: None,
        });
    }

    for call in &parsed.calls {
        let Some(source_key) = node_keys.get(call.source) else {
            continue;
        };
        let keys = call_keys(
            call,
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
                alias_key: None,
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

    let mut aliases = HashMap::<String, Candidate>::new();
    for (index, reference) in graph.refs.iter().enumerate() {
        check_progress(index, cancelled)?;
        let Some(alias) = reference.alias_key.as_ref() else {
            continue;
        };
        let target = reference_target(&reference.keys, &candidates, None)
            .map_or(Candidate::Ambiguous, Candidate::Unique);
        aliases
            .entry(alias.clone())
            .and_modify(|candidate| {
                if !matches!((*candidate, target), (Candidate::Unique(current), Candidate::Unique(next)) if current == next)
                {
                    *candidate = Candidate::Ambiguous;
                }
            })
            .or_insert(target);
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
    let mut edge_indices = HashMap::<(usize, usize, u8), usize>::new();
    let mut edges = Vec::<EdgeInput>::new();
    for (index, reference) in graph.refs.iter_mut().enumerate() {
        check_progress(index, cancelled)?;
        let alias_candidates = reference.alias_key.is_none().then_some(&aliases);
        let Some(target) = reference_target(&reference.keys, &candidates, alias_candidates) else {
            continue;
        };
        reference.resolved_target_key = Some(graph.nodes[target].key.clone());
        let source = node_by_key
            .get(reference.source_key.as_str())
            .copied()
            .ok_or_else(|| "reference source is missing".to_owned())?;

        let edge_kind = match reference.kind {
            RefKind::Imports => 2,
            RefKind::Calls => {
                if graph.nodes[source].kind == NodeKind::Test {
                    1
                } else {
                    0
                }
            }
        };
        let key = (source, target, edge_kind);
        if let Some(&edge) = edge_indices.get(&key) {
            edges[edge].support_count += 1;
        } else {
            let edge = edges.len();
            edge_indices.insert(key, edge);
            edges.push(EdgeInput {
                source_key: graph.nodes[key.0].key.clone(),
                target_key: graph.nodes[key.1].key.clone(),
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

fn reference_target(
    keys: &[String],
    candidates: &HashMap<&str, Candidate>,
    aliases: Option<&HashMap<String, Candidate>>,
) -> Option<usize> {
    for key in keys {
        let direct = candidates.get(key.as_str());
        let alias = aliases.and_then(|aliases| aliases.get(key));
        match (direct, alias) {
            (Some(Candidate::Ambiguous), _) | (_, Some(Candidate::Ambiguous)) => return None,
            (Some(Candidate::Unique(left)), Some(Candidate::Unique(right))) if left != right => {
                return None;
            }
            (Some(Candidate::Unique(node)), _) | (_, Some(Candidate::Unique(node))) => {
                return Some(*node);
            }
            (None, None) => {}
        }
    }
    None
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
    values: HashMap<usize, HashMap<String, Binding>>,
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
    call: &crate::parse::Call,
    parsed: &ParsedFile,
    paths: &[Option<String>],
    target: &TargetPath,
    module_paths: &[String],
    bindings: &Bindings,
) -> Vec<String> {
    let source = call.source;
    let definition = parsed.definitions.get(source);
    let module_index = definition.and_then(|definition| definition.module);
    let module = definition.map_or(target.module.as_str(), |definition| {
        lexical_module(definition.module, &target.module, module_paths)
    });
    let root = target.root.as_str();
    let Some(raw) = strip_generics(call.target.trim()) else {
        return Vec::new();
    };
    let raw = raw.as_ref();
    if raw.is_empty() {
        return Vec::new();
    }

    if let Some((receiver, method)) = raw.split_once('.') {
        if !valid_identifier(method) {
            return Vec::new();
        }
        let owner = if receiver == "self" {
            source_owner(source, parsed, paths).map(str::to_owned)
        } else if valid_identifier(receiver) {
            bindings
                .values
                .get(&source)
                .and_then(|values| values.get(receiver))
                .and_then(|binding| match binding {
                    Binding::Unique(type_target) => normalize_value_type(
                        type_target,
                        source,
                        module_index,
                        module,
                        root,
                        &bindings.imports,
                    ),
                    Binding::Ambiguous => None,
                })
        } else {
            None
        };
        return owner
            .map(|owner| vec![format!("rust:method:{}", join_path(&owner, method))])
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
            .is_some_and(|bindings| bindings.contains_key(name))
        {
            return vec![format!("rust:shadowed-value:{name}")];
        }
        if let Some(binding) = import_binding(&bindings.imports, source, module_index, name) {
            return match binding {
                Binding::Unique(path) => {
                    vec![format!("rust:function:{path}"), item_key(path)]
                }
                Binding::Ambiguous => vec![format!("rust:ambiguous-import:{name}")],
            };
        }
        let mut keys = Vec::with_capacity(4);
        if let Some(scope) = source_scope(source, parsed, paths, module) {
            let target = join_path(scope, name);
            keys.push(format!("rust:function:{target}"));
            keys.push(item_key(&target));
        }
        let target = join_path(module, name);
        keys.push(format!("rust:function:{target}"));
        keys.push(item_key(&target));
        if let Some(prefix) = glob_import_binding(&bindings.imports, source, module_index) {
            let target = join_path(prefix, name);
            keys.push(format!("rust:function:{target}"));
            keys.push(item_key(&target));
        }
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
    let mut owners = Vec::with_capacity(3);
    match import_binding(&bindings.imports, source, module_index, first) {
        Some(Binding::Unique(path)) => owners.push(if parts.len() == 2 {
            path.clone()
        } else {
            join_path(path, &parts[1..parts.len() - 1].join("::"))
        }),
        Some(Binding::Ambiguous) => {
            return vec![format!("rust:ambiguous-import:{owner}::{method}")];
        }
        None => {
            if !matches!(first, "crate" | "self" | "super")
                && let Some(scope) =
                    visible_local_type_scope(&owner, source, call.byte, parsed, paths)
            {
                owners.push(join_path(scope, &owner));
            }
            if let Some(local) = normalize_relative(&owner, module, root) {
                owners.push(local);
            }
            if !matches!(first, "crate" | "self" | "super")
                && let Some(prefix) = glob_import_binding(&bindings.imports, source, module_index)
            {
                owners.push(join_path(prefix, &owner));
            }
        }
    }
    let mut keys = Vec::with_capacity(owners.len().saturating_mul(3));
    for owner in owners {
        let target = join_path(&owner, method);
        keys.push(format!("rust:function:{target}"));
        keys.push(format!("rust:method:{target}"));
        keys.push(item_key(&target));
    }
    dedup_keys(keys)
}

fn visible_local_type_scope<'a>(
    owner: &str,
    source: usize,
    call_byte: usize,
    parsed: &ParsedFile,
    paths: &'a [Option<String>],
) -> Option<&'a str> {
    let scope = paths.get(source)?.as_deref()?;
    let target = join_path(scope, owner);
    parsed
        .definitions
        .iter()
        .enumerate()
        .any(|(index, definition)| {
            definition.kind == DefinitionKind::Type
                && definition.parent == Some(source)
                && definition
                    .block_scope
                    .is_some_and(|(start, end)| start <= call_byte && call_byte < end)
                && paths.get(index).and_then(Option::as_deref) == Some(target.as_str())
        })
        .then_some(scope)
}

fn normalize_value_type(
    raw: &str,
    source: usize,
    module_index: Option<usize>,
    module: &str,
    root: &str,
    imports: &ImportBindings,
) -> Option<String> {
    let target = strip_trailing_type_arguments(raw.trim())?;
    let parts = target.split("::").map(str::trim).collect::<Vec<_>>();
    let first = *parts.first()?;
    if !matches!(first, "crate" | "self" | "super")
        && let Some(binding) = import_binding(imports, source, module_index, first)
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

fn glob_import_binding(
    imports: &ImportBindings,
    source: usize,
    module: Option<usize>,
) -> Option<&str> {
    match import_binding(imports, source, module, "*") {
        Some(Binding::Unique(path)) => Some(path),
        Some(Binding::Ambiguous) | None => None,
    }
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
    if alias.is_none()
        && let Some(prefix) = path.strip_suffix("::*")
    {
        return Some(("*".into(), normalize_relative(prefix, module, root)?));
    }
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

    fn from_parse_context(context: &str) -> Result<Self, String> {
        let (length, value) = context
            .split_once(':')
            .ok_or_else(|| "captured Rust parse context is invalid".to_owned())?;
        let root_length = length
            .parse::<usize>()
            .ok()
            .filter(|length| *length <= value.len() && value.is_char_boundary(*length))
            .ok_or_else(|| "captured Rust parse context is invalid".to_owned())?;
        let (root, module) = value.split_at(root_length);
        Ok(Self {
            root: root.to_owned(),
            module: module.to_owned(),
        })
    }
}

#[derive(Clone, Copy, Default)]
struct PackageLayout {
    exists: bool,
    library: bool,
    main: bool,
}

struct TargetLayout {
    packages: HashMap<String, PackageLayout>,
}

impl TargetLayout {
    fn from_inventory<'a>(
        source_paths: impl IntoIterator<Item = &'a str>,
        cargo_manifest_paths: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut packages = HashMap::<String, PackageLayout>::new();
        packages.entry(String::new()).or_default().exists = true;
        for path in cargo_manifest_paths {
            let package = if path == "Cargo.toml" {
                Some("")
            } else {
                path.strip_suffix("/Cargo.toml")
            };
            if let Some(package) = package {
                packages.entry(package.to_owned()).or_default().exists = true;
            }
        }
        for path in source_paths {
            for (suffix, library) in [("src/lib.rs", true), ("src/main.rs", false)] {
                let package = if path == suffix {
                    Some("")
                } else {
                    path.strip_suffix(&format!("/{suffix}"))
                };
                if let Some(package) = package {
                    let package = packages.entry(package.to_owned()).or_default();
                    if library {
                        package.library = true;
                    } else {
                        package.main = true;
                    }
                }
            }
        }
        Self { packages }
    }

    #[cfg(test)]
    fn from_sources(sources: &[Source]) -> Self {
        let manifests = sources
            .iter()
            .filter_map(|source| {
                ["src/lib.rs", "src/main.rs"]
                    .into_iter()
                    .find_map(|suffix| {
                        source
                            .path
                            .strip_suffix(&format!("/{suffix}"))
                            .map(|package| format!("{package}/Cargo.toml"))
                    })
            })
            .collect::<Vec<_>>();
        Self::from_inventory(
            sources.iter().map(|source| source.path.as_str()),
            manifests.iter().map(String::as_str),
        )
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
        self.packages.get(package).copied().unwrap_or_default()
    }
}

pub(crate) fn assign_parse_contexts(
    files: &mut [CapturedSource],
    cargo_manifest_paths: &BTreeSet<String>,
) {
    let mut targets = TargetLayout::from_inventory(
        files.iter().map(|file| file.path.as_str()),
        cargo_manifest_paths.iter().map(String::as_str),
    );
    for file in files {
        file.parse_context = match file.language {
            Language::Rust => targets.for_path(&file.path).parse_context(),
            Language::Python => String::new(),
        };
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
    use crate::artifact::AnalyzerKind;
    use crate::git::{ArtifactFile, ArtifactOmission, ArtifactReview, ChangeLayer};
    use crate::workspace::{
        AllowedRoots, ErrorCode, IndexRequest, SnapshotTarget, resolve_request,
    };
    use std::fs;
    use std::process::Command;

    fn complete_artifact(path: &str, analyzer: AnalyzerKind) -> ArtifactFile {
        ArtifactFile {
            path: path.into(),
            analyzer,
            diff_complete: true,
            analysis_complete: true,
            omission: None,
        }
    }

    #[test]
    fn exact_graph_hit_reuses_every_file() {
        let root = snapshot_repository("exact-graph-hit");
        let engine = Engine::new(Arc::new(AllowedRoots::new(vec![root.clone()]).unwrap()));
        let request = snapshot_request(&engine, &root, "HEAD", "HEAD");

        let first = engine
            .build_snapshot(request.clone(), &AtomicBool::new(false), |_| {})
            .unwrap();
        let second = engine
            .build_snapshot(request, &AtomicBool::new(false), |_| {})
            .unwrap();

        assert_eq!(first.graph_image_id, second.graph_image_id);
        assert_eq!(second.stats.files_total, 2);
        assert_eq!(second.stats.files_reused, 2);
        assert_eq!(second.stats.files_parsed, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn base_oid_seed_reparses_only_changed_files() {
        let root = snapshot_repository("base-oid-seed");
        let base = test_git_line(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join("src/a.rs"), "fn changed() {}\n").unwrap();
        test_git(&root, &["commit", "--quiet", "-am", "change one file"]);
        let head = test_git_line(&root, &["rev-parse", "HEAD"]);
        let engine = Engine::new(Arc::new(AllowedRoots::new(vec![root.clone()]).unwrap()));
        engine
            .build_snapshot(
                snapshot_request(&engine, &root, &base, &base),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();

        let completion = engine
            .build_snapshot(
                snapshot_request(&engine, &root, &base, &head),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();

        assert_eq!(completion.stats.files_total, 2);
        assert_eq!(completion.stats.files_reused, 1);
        assert_eq!(completion.stats.files_parsed, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_non_exact_seed_is_quarantined_and_reported() {
        let root = snapshot_repository("corrupt-seed");
        let base = test_git_line(&root, &["rev-parse", "HEAD"]);
        let engine = Engine::new(Arc::new(AllowedRoots::new(vec![root.clone()]).unwrap()));
        let first = engine
            .build_snapshot(
                snapshot_request(&engine, &root, &base, &base),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let graph = engine
            .snapshot(&first.snapshot_id)
            .unwrap()
            .graph_path
            .clone();
        fs::set_permissions(&graph, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        fs::write(root.join("src/a.rs"), "fn changed() {}\n").unwrap();
        test_git(&root, &["commit", "--quiet", "-am", "change one file"]);
        let head = test_git_line(&root, &["rev-parse", "HEAD"]);
        let rejected = Mutex::new(Vec::new());

        let completion = engine
            .build_snapshot(
                snapshot_request(&engine, &root, &base, &head),
                &AtomicBool::new(false),
                |progress| {
                    if let Some(path) = progress.rejected_cache {
                        rejected.lock().unwrap().push(path);
                    }
                },
            )
            .unwrap();

        assert_eq!(completion.stats.files_parsed, 2);
        assert!(!rejected.into_inner().unwrap().is_empty());
        assert!(
            fs::read_dir(root.join(".git/graphr/v6/quarantine"))
                .unwrap()
                .next()
                .is_some()
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn snapshot_repository(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "graphr-index-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        test_git(&root, &["init", "--quiet", "--initial-branch=main"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(root.join("src/a.rs"), "fn first() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "fn second() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "base"]);
        root
    }

    fn snapshot_request(
        engine: &Engine,
        root: &Path,
        base: &str,
        head: &str,
    ) -> crate::workspace::ResolvedIndexRequest {
        resolve_request(
            engine.roots(),
            IndexRequest {
                worktree_root: root.to_path_buf(),
                base_ref: base.into(),
                head_ref: head.into(),
                target: SnapshotTarget::Commit,
                dependency_mode: DependencyMode::Boundary,
            },
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    fn test_git_line(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    #[test]
    fn legacy_project_accepts_subdirectory_while_workspace_rejects_it() {
        let root = std::env::temp_dir().join(format!(
            "graphr-index-subdirectory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(root.join("baseline.txt"), "baseline\n").unwrap();
        test_git(&root, &["add", "--", "baseline.txt"]);
        test_git(&root, &["commit", "--quiet", "-m", "baseline"]);

        assert!(Project::open(&nested).is_ok());
        assert_eq!(
            AllowedRoots::new(vec![root.clone()])
                .unwrap()
                .inspect(&nested, &AtomicBool::new(false))
                .unwrap_err()
                .code,
            ErrorCode::RootNotWorktree
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_project_indexes_an_unborn_worktree() {
        let root = std::env::temp_dir().join(format!(
            "graphr-index-unborn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        test_git(&root, &["init", "--quiet"]);
        fs::write(root.join("src/lib.rs"), "pub fn unborn() {}\n").unwrap();

        let output = Project::open(&root).unwrap().index(false).unwrap();

        assert!(output.contains("changed=1"), "{output}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_project_skips_an_oversized_clean_git_blob() {
        let root = std::env::temp_dir().join(format!(
            "graphr-index-oversized-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(root.join("a-large.rs"), vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
        fs::write(root.join("z-small.rs"), "pub fn retained() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "baseline"]);

        let project = Project::open(&root).unwrap();
        let output = project.index(false).unwrap();

        assert!(output.contains("changed=1"), "{output}");
        assert!(output.contains("skipped=1"), "{output}");
        assert!(
            project
                .search("retained", None, 20)
                .unwrap()
                .contains("retained")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn index_progress_counts_reuse_before_pending_parse_completion() {
        use std::os::unix::fs::DirBuilderExt;

        let root = std::env::temp_dir().join(format!(
            "graphr-index-progress-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(root.join("a.rs"), "fn first() {}\n").unwrap();
        fs::write(root.join("b.rs"), "fn reused() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let first_capture = root.join("first-capture");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&first_capture)
            .unwrap();
        let first = repository
            .capture_sources(
                &repository.head_oid,
                &SnapshotTarget::Commit,
                &first_capture,
                &AtomicBool::new(false),
            )
            .unwrap();
        let mut store =
            Store::open(&root.join("graph/index.db"), false, &AtomicBool::new(false)).unwrap();
        store
            .index_with(&AtomicBool::new(false), |full, existing| {
                build_index(
                    &repository,
                    &first,
                    &AtomicBool::new(false),
                    full,
                    existing,
                    |_, _, _| {},
                )
            })
            .unwrap();

        fs::write(root.join("a.rs"), "fn second() {}\n").unwrap();
        let second_capture = root.join("second-capture");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&second_capture)
            .unwrap();
        let second = repository
            .capture_sources(
                &repository.head_oid,
                &SnapshotTarget::Worktree {
                    include_untracked: false,
                },
                &second_capture,
                &AtomicBool::new(false),
            )
            .unwrap();
        let events = Mutex::new(Vec::new());
        store
            .index_with(&AtomicBool::new(false), |full, existing| {
                build_index(
                    &repository,
                    &second,
                    &AtomicBool::new(false),
                    full,
                    existing,
                    |done, total, reused| events.lock().unwrap().push((done, total, reused)),
                )
            })
            .unwrap();

        assert_eq!(
            events.into_inner().unwrap(),
            [(0, 2, 0), (1, 2, 1), (2, 2, 1)]
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parallel_index_progress_stays_monotonic_when_the_first_callback_is_delayed() {
        use std::os::unix::fs::DirBuilderExt;
        use std::time::Duration;

        let root = std::env::temp_dir().join(format!(
            "graphr-index-parallel-progress-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        for index in 0..8 {
            fs::write(
                root.join(format!("file-{index}.rs")),
                format!("pub fn file_{index}() {{}}\n"),
            )
            .unwrap();
        }
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let capture = root.join("capture");
        fs::DirBuilder::new().mode(0o700).create(&capture).unwrap();
        let sources = repository
            .capture_sources(
                &repository.head_oid,
                &SnapshotTarget::Commit,
                &capture,
                &AtomicBool::new(false),
            )
            .unwrap();
        let events = Mutex::new(Vec::new());

        build_index(
            &repository,
            &sources,
            &AtomicBool::new(false),
            true,
            &HashMap::new(),
            |done, _, _| {
                if done == 1 {
                    thread::sleep(Duration::from_millis(100));
                }
                events.lock().unwrap().push(done);
            },
        )
        .unwrap();

        assert_eq!(events.into_inner().unwrap(), (0..=8).collect::<Vec<_>>());
        fs::remove_dir_all(root).unwrap();
    }

    fn test_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
    }

    #[test]
    fn artifact_pages_are_independent_reconstructable_and_complete() {
        let artifact_patch = format!(
            "diff --git a/README.md b/README.md\n@@ -1 +1 @@\n-old\n+{}\n",
            "é".repeat(5_000)
        );
        let changes = WorktreeChanges {
            files: vec![],
            records: vec![],
            paths: vec![ChangedPath {
                status: ChangeStatus::Modified,
                old_path: None,
                old_language: None,
                path: "README.md".into(),
                language: None,
                additions: Some(1),
                deletions: Some(1),
                layers: Vec::new(),
            }],
            source_patch: String::new(),
            artifacts: ArtifactReview {
                files: vec![complete_artifact("README.md", AnalyzerKind::Markdown)],
                analysis:
                    "markdown path=\"README.md\" change=added kind=requirement value=\"REQ-2\" line=1\n"
                        .into(),
                patch: artifact_patch.clone(),
            },
            skipped_paths: 0,
        };
        let graph = "risk overall=0.0000 changed_symbols_total=0 changed_symbols_analyzed=0 changed_symbols_emitted=0 changed_symbols_omitted=0 flows_total=0 static_test_path_gaps=0 analysis_complete=true analysis_roots_omitted=0 deleted_paths_unanalyzed=0 neighborhood_omitted=false unmapped_ranges=0\n";
        let snapshot = ReviewSnapshot::new(
            "HEAD",
            6,
            50,
            DependencyMode::Boundary,
            changes,
            graph.into(),
        );

        let initial = review_context(&snapshot).unwrap();
        assert!(initial.contains("\nartifacts\n"), "{initial}");
        assert!(initial.contains("artifacts_next_cursor="), "{initial}");
        assert!(initial.contains("review_complete_when_pages_exhausted=true"));
        let mut stale = next_cursor(&initial, "artifacts_next_cursor").unwrap();
        let replacement = if stale.ends_with('0') { "1" } else { "0" };
        stale.replace_range(stale.len() - 1.., replacement);
        assert_eq!(
            render_section(&snapshot, &parse_review_cursor(&stale).unwrap()).unwrap_err(),
            "stale changes cursor"
        );

        let expected = format!(
            "artifact path=\"README.md\" analyzer=markdown diff_complete=true analysis_complete=true\n{}{}",
            snapshot.changes.artifacts.analysis, artifact_patch
        );
        let mut reconstructed = String::new();
        let mut offset = 0;
        loop {
            let (page, more) = render_section_page(
                &snapshot,
                ReviewSection::Artifacts,
                offset,
                SECTION_OVERHEAD + 257,
            )
            .unwrap();
            let metadata = page.lines().nth(1).unwrap();
            let emitted = metadata
                .split_ascii_whitespace()
                .find_map(|field| field.strip_prefix("emitted_bytes="))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let content_start = page.match_indices('\n').nth(1).unwrap().0 + 1;
            reconstructed.push_str(&page[content_start..content_start + emitted]);
            offset += emitted;
            if !more {
                break;
            }
        }
        assert_eq!(reconstructed, expected);
    }

    #[test]
    fn artifact_text_frames_analysis_and_patch_with_one_newline() {
        for (analysis, patch, expected) in [
            ("semantic", "diff", "semantic\ndiff"),
            ("semantic\n", "diff", "semantic\ndiff"),
            ("", "diff", "diff"),
            ("semantic", "", "semantic"),
        ] {
            let review = ArtifactReview {
                files: vec![],
                analysis: analysis.into(),
                patch: patch.into(),
            };
            assert_eq!(artifact_text(&review), expected);
        }
    }

    #[test]
    fn omitted_artifact_keeps_review_incomplete() {
        let mut file = complete_artifact("image.bin", AnalyzerKind::Generic);
        file.diff_complete = false;
        file.analysis_complete = false;
        file.omission = Some(ArtifactOmission::Binary);
        let changes = WorktreeChanges {
            files: vec![],
            records: vec![],
            paths: vec![ChangedPath {
                status: ChangeStatus::Modified,
                old_path: None,
                old_language: None,
                path: "image.bin".into(),
                language: None,
                additions: None,
                deletions: None,
                layers: Vec::new(),
            }],
            source_patch: String::new(),
            artifacts: ArtifactReview {
                files: vec![file],
                analysis: String::new(),
                patch: String::new(),
            },
            skipped_paths: 0,
        };
        assert!(!change_content_complete(&changes, DependencyMode::Boundary));
    }

    #[test]
    fn source_looking_nonregular_path_is_not_source_complete() {
        let changes = WorktreeChanges {
            files: vec![],
            records: vec![],
            paths: vec![ChangedPath {
                status: ChangeStatus::Modified,
                old_path: None,
                old_language: None,
                path: "link.rs".into(),
                language: None,
                additions: None,
                deletions: None,
                layers: Vec::new(),
            }],
            source_patch: String::new(),
            artifacts: ArtifactReview {
                files: vec![ArtifactFile {
                    path: "link.rs".into(),
                    analyzer: AnalyzerKind::Generic,
                    diff_complete: false,
                    analysis_complete: false,
                    omission: Some(ArtifactOmission::NonRegular),
                }],
                analysis: String::new(),
                patch: String::new(),
            },
            skipped_paths: 0,
        };

        assert!(!path_in_source_patch(
            &changes.paths[0],
            DependencyMode::Boundary
        ));
        assert!(!change_content_complete(&changes, DependencyMode::Boundary));
    }

    #[test]
    fn review_context_pages_diff_and_graph_without_losing_utf8() {
        let patch = format!(
            "diff --git a/src/lib.rs b/src/lib.rs\n{}sentinel-last-hunk\n",
            "@@ -1 +1 @@\n-é old\n+é changed\n".repeat(400)
        );
        let graph = format!(
            "risk overall=0.3000 changed_symbols_total=1 changed_symbols_analyzed=1 changed_symbols_emitted=1 changed_symbols_omitted=0 flows_total=300 static_test_path_gaps=1 analysis_complete=true neighborhood_omitted=false unmapped_ranges=0\n{}",
            (0..300)
                .map(|index| format!(
                    "flow 0.1000 entry_{index}@src/lib.rs:1 -> changed@src/lib.rs:2\n"
                ))
                .collect::<String>()
        );
        let changes = WorktreeChanges {
            files: vec![],
            records: vec![],
            paths: vec![ChangedPath {
                status: ChangeStatus::Modified,
                old_path: None,
                old_language: None,
                path: "src/lib.rs".into(),
                language: Some(Language::Rust),
                additions: Some(400),
                deletions: Some(400),
                layers: Vec::new(),
            }],
            source_patch: patch,
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        let snapshot = ReviewSnapshot::new("HEAD", 6, 50, DependencyMode::Boundary, changes, graph);
        assert!(snapshot.matches("HEAD", 6, 50, DependencyMode::Boundary));
        assert!(!snapshot.matches("HEAD", 6, 50, DependencyMode::Full));
        let initial = review_context(&snapshot).unwrap();
        assert!(initial.len() <= REVIEW_CONTEXT_BUDGET);
        assert!(initial.contains("emitted_entries=1 partial_entries=0 total_entries=1"));
        assert!(initial.contains("rename_detection=within-source-and-artifact"));
        assert!(initial.contains("total_hunks=400"));
        assert!(initial.contains("total_flows=300"));
        assert!(
            initial.contains("review_complete=false review_complete_when_pages_exhausted=true")
        );
        assert!(!initial.contains("[truncated]"));

        let mut pages = initial.clone();
        for label in [
            "diff_next_cursor",
            "artifacts_next_cursor",
            "graph_next_cursor",
        ] {
            let mut cursor = next_cursor(&initial, label);
            for _ in 0..100 {
                let Some(token) = cursor else { break };
                let output =
                    render_section(&snapshot, &parse_review_cursor(&token).unwrap()).unwrap();
                assert!(output.len() <= REVIEW_CONTEXT_BUDGET);
                assert!(output.is_char_boundary(output.len()));
                assert!(!output.contains("[truncated]"));
                assert!(output.contains("review_complete_when_pages_exhausted=true"));
                cursor = next_cursor(&output, label);
                pages.push_str(&output);
            }
            assert!(cursor.is_none(), "pagination did not finish");
        }
        assert!(pages.contains("sentinel-last-hunk"));
        assert_eq!(
            pages
                .lines()
                .filter(|line| line.starts_with("flow "))
                .count(),
            300
        );

        let mut stale = next_cursor(&initial, "diff_next_cursor").unwrap();
        let replacement = if stale.ends_with('0') { "1" } else { "0" };
        stale.replace_range(stale.len() - 1.., replacement);
        assert_eq!(
            render_section(&snapshot, &parse_review_cursor(&stale).unwrap()).unwrap_err(),
            "stale changes cursor"
        );
    }

    #[test]
    fn max_nodes_limits_each_graph_page_not_the_snapshot() {
        let graph = format!(
            "risk overall=0.3000 changed_symbols_total=6 changed_symbols_analyzed=6 changed_symbols_emitted=6 changed_symbols_omitted=0 flows_total=0 static_test_path_gaps=6 analysis_complete=true neighborhood_omitted=false unmapped_ranges=0\n{}",
            (0..6)
                .map(|index| format!("  risk 0.3000 node-{index}\n"))
                .collect::<String>()
        );
        let snapshot = ReviewSnapshot::new(
            "HEAD",
            6,
            2,
            DependencyMode::Boundary,
            WorktreeChanges {
                files: vec![],
                records: vec![],
                paths: vec![ChangedPath {
                    status: ChangeStatus::Modified,
                    old_path: None,
                    old_language: None,
                    path: "src/lib.rs".into(),
                    language: Some(Language::Rust),
                    additions: Some(1),
                    deletions: Some(1),
                    layers: Vec::new(),
                }],
                source_patch: "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n"
                    .into(),
                artifacts: Default::default(),
                skipped_paths: 0,
            },
            graph,
        );
        let initial = review_context(&snapshot).unwrap();
        assert!(
            initial.contains("page_record_limit=2 emitted_records=2"),
            "{initial}"
        );
        assert!(initial.contains("node-0"), "{initial}");
        assert!(!initial.contains("node-1"), "{initial}");

        let mut pages = initial.clone();
        let mut cursor = next_cursor(&initial, "graph_next_cursor");
        while let Some(token) = cursor {
            let output = render_section(&snapshot, &parse_review_cursor(&token).unwrap()).unwrap();
            assert!(
                output.lines().filter(|line| line.contains("node-")).count() <= 2,
                "{output}"
            );
            cursor = next_cursor(&output, "graph_next_cursor");
            pages.push_str(&output);
        }
        for index in 0..6 {
            assert!(pages.contains(&format!("node-{index}")), "{pages}");
        }
    }

    #[test]
    fn review_complete_rejects_omitted_changed_symbols() {
        let changes = WorktreeChanges {
            files: vec![],
            records: vec![],
            paths: vec![ChangedPath {
                status: ChangeStatus::Modified,
                old_path: None,
                old_language: None,
                path: "src/lib.rs".into(),
                language: Some(Language::Rust),
                additions: Some(1),
                deletions: Some(1),
                layers: Vec::new(),
            }],
            source_patch: "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n".into(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        let graph = "risk overall=0.3000 changed_symbols_total=2 changed_symbols_analyzed=2 changed_symbols_emitted=1 changed_symbols_omitted=1 flows_total=0 static_test_path_gaps=0 analysis_complete=true neighborhood_omitted=false unmapped_ranges=0\n";

        let snapshot = ReviewSnapshot::new(
            "HEAD",
            6,
            50,
            DependencyMode::Boundary,
            changes,
            graph.into(),
        );
        let output = review_context(&snapshot).unwrap();

        assert!(output.contains("review_complete=false"), "{output}");
        assert!(
            output.contains("review_complete_when_pages_exhausted=false"),
            "{output}"
        );
    }

    #[test]
    fn cross_class_rename_is_explicit_and_incomplete() {
        let changes = WorktreeChanges {
            files: vec![],
            records: vec![],
            paths: vec![ChangedPath {
                status: ChangeStatus::Renamed,
                old_path: Some("src/old.rs".into()),
                old_language: Some(Language::Rust),
                path: "tests/fixture.tsv".into(),
                language: None,
                additions: Some(0),
                deletions: Some(0),
                layers: Vec::new(),
            }],
            source_patch: "diff --git a/src/old.rs b/tests/fixture.tsv\n".into(),
            artifacts: ArtifactReview {
                files: vec![ArtifactFile {
                    path: "tests/fixture.tsv".into(),
                    analyzer: AnalyzerKind::Tsv,
                    diff_complete: false,
                    analysis_complete: false,
                    omission: Some(ArtifactOmission::TypeChanged),
                }],
                analysis: String::new(),
                patch: String::new(),
            },
            skipped_paths: 0,
        };
        let graph = "risk overall=0.0000 changed_symbols_total=0 changed_symbols_analyzed=0 changed_symbols_emitted=0 changed_symbols_omitted=0 flows_total=0 static_test_path_gaps=0 analysis_complete=true neighborhood_omitted=false unmapped_ranges=0\n";

        let snapshot = ReviewSnapshot::new(
            "HEAD",
            6,
            50,
            DependencyMode::Boundary,
            changes,
            graph.into(),
        );
        let output = review_context(&snapshot).unwrap();

        assert!(
            output.contains("renamed artifact omitted src/old.rs -> tests/fixture.tsv analyzer=tsv reason=type-changed"),
            "{output}"
        );
        assert!(output.contains("review_complete=false"), "{output}");
        assert!(
            output.contains("review_complete_when_pages_exhausted=false"),
            "{output}"
        );
    }

    #[test]
    fn graph_completeness_reads_only_the_summary() {
        let graph = "risk overall=0.0000 changed_symbols_total=0 changed_symbols_analyzed=0 changed_symbols_emitted=0 changed_symbols_omitted=0 flows_total=0 static_test_path_gaps=0 analysis_complete=true neighborhood_omitted=false unmapped_ranges=0\n  risk 0.0000 node src/analysis_complete=false.rs:1\n";
        assert!(graph_flow_analysis_complete(graph));
        assert!(graph_review_complete(graph));
    }

    #[test]
    fn diff_metadata_separates_patch_and_all_path_totals() {
        let changes = WorktreeChanges {
            files: vec![],
            records: vec![],
            paths: vec![
                ChangedPath {
                    status: ChangeStatus::Modified,
                    old_path: None,
                    old_language: None,
                    path: "src/lib.rs".into(),
                    language: Some(Language::Rust),
                    additions: Some(436),
                    deletions: Some(7),
                    layers: Vec::new(),
                },
                ChangedPath {
                    status: ChangeStatus::Untracked,
                    old_path: None,
                    old_language: None,
                    path: "tests/fixture.tsv".into(),
                    language: None,
                    additions: Some(3),
                    deletions: Some(0),
                    layers: Vec::new(),
                },
            ],
            source_patch: "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n".into(),
            artifacts: ArtifactReview {
                files: vec![complete_artifact("tests/fixture.tsv", AnalyzerKind::Tsv)],
                analysis: String::new(),
                patch: "diff --git a/tests/fixture.tsv b/tests/fixture.tsv\n@@ -0,0 +1,3 @@\n+a\tb\n+1\t2\n+3\t4\n"
                    .into(),
            },
            skipped_paths: 0,
        };
        let graph = "risk overall=0.3000 changed_symbols_total=1 changed_symbols_analyzed=1 changed_symbols_emitted=1 changed_symbols_omitted=0 flows_total=0 static_test_path_gaps=0 analysis_complete=true neighborhood_omitted=false unmapped_ranges=0\n";

        let snapshot = ReviewSnapshot::new(
            "HEAD",
            6,
            50,
            DependencyMode::Boundary,
            changes,
            graph.into(),
        );
        let output = review_context(&snapshot).unwrap();

        let diff_metadata = output
            .lines()
            .find(|line| line.starts_with("diff scope="))
            .unwrap();
        assert!(
            diff_metadata.contains("patch_additions=436 patch_deletions=7"),
            "{diff_metadata}"
        );
        assert!(
            diff_metadata.contains("all_path_additions=439 all_path_deletions=7"),
            "{diff_metadata}"
        );
        assert!(
            diff_metadata.contains("all_path_hunks=2"),
            "{diff_metadata}"
        );
        let artifact_metadata = output
            .lines()
            .find(|line| line.starts_with("artifacts emitted_bytes="))
            .unwrap();
        assert!(
            artifact_metadata.contains("patch_additions=3 patch_deletions=0"),
            "{artifact_metadata}"
        );

        let unknown = WorktreeChanges {
            files: vec![],
            records: vec![],
            paths: vec![ChangedPath {
                status: ChangeStatus::Deleted,
                old_path: None,
                old_language: None,
                path: "src/old.rs".into(),
                language: Some(Language::Rust),
                additions: None,
                deletions: None,
                layers: Vec::new(),
            }],
            source_patch: String::new(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        assert_eq!(
            change_hunk_totals(&unknown, 0, 0, DependencyMode::Boundary),
            "all_path_hunks_at_least=0 all_path_unknown_hunk_paths=1"
        );
    }

    #[test]
    fn untracked_source_is_complete_diff_content() {
        let changes = WorktreeChanges {
            files: vec![],
            records: vec![crate::git::PathRecord::Untracked("src/new.rs".into())],
            paths: vec![ChangedPath {
                status: ChangeStatus::Untracked,
                old_path: None,
                old_language: None,
                path: "src/new.rs".into(),
                language: Some(Language::Rust),
                additions: Some(1),
                deletions: Some(0),
                layers: Vec::new(),
            }],
            source_patch: "diff --git a/src/new.rs b/src/new.rs\n@@ -0,0 +1 @@\n+fn new() {}\n"
                .into(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        let graph = "risk overall=0.3000 changed_symbols_total=1 changed_symbols_analyzed=1 changed_symbols_emitted=1 changed_symbols_omitted=0 flows_total=0 static_test_path_gaps=1 analysis_complete=true neighborhood_omitted=false unmapped_ranges=0\n";
        let output = review_context(&ReviewSnapshot::new(
            "HEAD",
            0,
            1,
            DependencyMode::Boundary,
            changes,
            graph.into(),
        ))
        .unwrap();

        assert!(output.contains("scope=source"), "{output}");
        assert!(
            output.contains("patch_additions=1 patch_deletions=0"),
            "{output}"
        );
        assert!(output.contains("all_path_hunks=1"), "{output}");
        assert!(
            output.contains("review_complete=true review_complete_when_pages_exhausted=true"),
            "{output}"
        );
    }

    #[test]
    fn change_manifest_preserves_every_path_and_status() {
        let mut large = complete_artifact("LARGE.md", AnalyzerKind::Markdown);
        large.analysis_complete = false;
        large.omission = Some(ArtifactOmission::Oversized);
        let mut binary = complete_artifact("image.bin", AnalyzerKind::Generic);
        binary.diff_complete = false;
        binary.analysis_complete = false;
        binary.omission = Some(ArtifactOmission::Binary);
        let changes = WorktreeChanges {
            files: vec![],
            records: vec![],
            paths: vec![
                ChangedPath {
                    status: ChangeStatus::Modified,
                    old_path: None,
                    old_language: None,
                    path: "src/current.rs".into(),
                    language: Some(Language::Rust),
                    additions: Some(2),
                    deletions: Some(1),
                    layers: vec![ChangeLayer::Committed, ChangeLayer::Staged],
                },
                ChangedPath {
                    status: ChangeStatus::Modified,
                    old_path: None,
                    old_language: None,
                    path: "pkg/app.py".into(),
                    language: Some(Language::Python),
                    additions: Some(1),
                    deletions: Some(1),
                    layers: vec![ChangeLayer::Committed],
                },
                ChangedPath {
                    status: ChangeStatus::Modified,
                    old_path: None,
                    old_language: None,
                    path: "README.md".into(),
                    language: None,
                    additions: Some(1),
                    deletions: Some(1),
                    layers: vec![ChangeLayer::Staged],
                },
                ChangedPath {
                    status: ChangeStatus::Modified,
                    old_path: None,
                    old_language: None,
                    path: "LARGE.md".into(),
                    language: None,
                    additions: Some(1),
                    deletions: Some(1),
                    layers: vec![ChangeLayer::Unstaged],
                },
                ChangedPath {
                    status: ChangeStatus::Untracked,
                    old_path: None,
                    old_language: None,
                    path: "tests/fixture.tsv".into(),
                    language: None,
                    additions: Some(3),
                    deletions: Some(0),
                    layers: vec![ChangeLayer::Untracked],
                },
                ChangedPath {
                    status: ChangeStatus::Modified,
                    old_path: None,
                    old_language: None,
                    path: "image.bin".into(),
                    language: None,
                    additions: None,
                    deletions: None,
                    layers: vec![ChangeLayer::Unstaged],
                },
                ChangedPath {
                    status: ChangeStatus::Deleted,
                    old_path: None,
                    old_language: None,
                    path: "src/deleted.rs".into(),
                    language: Some(Language::Rust),
                    additions: Some(0),
                    deletions: Some(3),
                    layers: vec![ChangeLayer::Staged],
                },
                ChangedPath {
                    status: ChangeStatus::Renamed,
                    old_path: Some("src/old.rs".into()),
                    old_language: Some(Language::Rust),
                    path: "src/new.rs".into(),
                    language: Some(Language::Rust),
                    additions: Some(0),
                    deletions: Some(0),
                    layers: vec![ChangeLayer::Committed],
                },
            ],
            source_patch: String::new(),
            artifacts: ArtifactReview {
                files: vec![
                    large,
                    complete_artifact("README.md", AnalyzerKind::Markdown),
                    binary,
                    complete_artifact("tests/fixture.tsv", AnalyzerKind::Tsv),
                ],
                analysis: String::new(),
                patch: "diff --git a/tests/fixture.tsv b/tests/fixture.tsv\n@@ -0,0 +1,3 @@\n+a\tb\n+1\t2\n+3\t4\n"
                    .into(),
            },
            skipped_paths: 2,
        };
        assert_eq!(
            change_manifest(&changes, DependencyMode::Boundary),
            "changed source rust src/current.rs status=modified additions=2 deletions=1 layers=committed,staged\nchanged source python pkg/app.py status=modified additions=1 deletions=1 layers=committed\nchanged artifact text README.md analyzer=markdown additions=1 deletions=1 layers=staged\nchanged artifact text LARGE.md analyzer=markdown analysis=omitted reason=oversized additions=1 deletions=1 layers=unstaged\nuntracked artifact text tests/fixture.tsv analyzer=tsv additions=3 deletions=0 layers=untracked\nchanged artifact omitted image.bin analyzer=generic reason=binary additions=unknown deletions=unknown layers=unstaged\ndeleted source rust src/deleted.rs additions=0 deletions=3 layers=staged\nrenamed source rust src/old.rs -> src/new.rs additions=0 deletions=0 layers=committed\nskipped 2 unsafe paths\n"
        );
    }

    #[test]
    fn review_snapshot_retains_shared_changes() {
        let changes = Arc::new(WorktreeChanges {
            files: Vec::new(),
            records: Vec::new(),
            paths: Vec::new(),
            source_patch: String::new(),
            artifacts: ArtifactReview::default(),
            skipped_paths: 0,
        });
        let snapshot = ReviewSnapshot::new(
            "HEAD",
            0,
            1,
            DependencyMode::Boundary,
            Arc::clone(&changes),
            String::new(),
        );

        assert!(Arc::ptr_eq(&snapshot.changes, &changes));
    }

    #[test]
    fn byte_pages_reconstruct_oversized_unicode_lines_exactly() {
        let source = format!("first\n{}\nlast\n", "é".repeat(5_000));
        let snapshot = ReviewSnapshot::new(
            "HEAD",
            6,
            50,
            DependencyMode::Boundary,
            WorktreeChanges {
                files: vec![],
                records: vec![],
                paths: vec![ChangedPath {
                    status: ChangeStatus::Modified,
                    old_path: None,
                    old_language: None,
                    path: "src/lib.rs".into(),
                    language: Some(Language::Rust),
                    additions: Some(1),
                    deletions: Some(1),
                layers: Vec::new(),
                }],
                source_patch: source.clone(),
                artifacts: Default::default(),
                skipped_paths: 0,
            },
            "risk overall=0.0000 changed_symbols_total=0 changed_symbols_analyzed=0 changed_symbols_emitted=0 changed_symbols_omitted=0 flows_total=0 static_test_path_gaps=0 analysis_complete=true analysis_roots_omitted=0 deleted_paths_unanalyzed=0 neighborhood_omitted=false unmapped_ranges=0\n".into(),
        );
        let mut offset = 0;
        let mut reconstructed = String::new();
        while offset < source.len() {
            let (rendered, more) = render_section_page(
                &snapshot,
                ReviewSection::Diff,
                offset,
                SECTION_OVERHEAD + 257,
            )
            .unwrap();
            let metadata = rendered.lines().nth(1).unwrap();
            let field = |name: &str| {
                metadata
                    .split_ascii_whitespace()
                    .find_map(|field| field.strip_prefix(&format!("{name}=")))
                    .unwrap()
            };
            let emitted = field("emitted_bytes").parse::<usize>().unwrap();
            let (start, end) = field("byte_range").split_once("..").unwrap();
            let start = start.parse::<usize>().unwrap();
            let end = end.parse::<usize>().unwrap();
            assert_eq!(start, offset);
            assert_eq!(end - start, emitted);
            let content_start = rendered.match_indices('\n').nth(1).unwrap().0 + 1;
            let content_end = content_start + emitted;
            assert!(rendered.is_char_boundary(content_end));
            let content = &rendered[content_start..content_end];
            assert_eq!(content, &source[start..end]);
            let framing = field("framing_suffix_bytes").parse::<usize>().unwrap();
            assert_eq!(framing, usize::from(!content.ends_with('\n')));
            if framing == 1 {
                assert_eq!(rendered.as_bytes().get(content_end), Some(&b'\n'));
            }
            reconstructed.push_str(content);
            offset = end;
            assert_eq!(more, offset < source.len());
        }
        assert_eq!(reconstructed, source);
    }

    fn next_cursor(output: &str, label: &str) -> Option<String> {
        output.lines().find_map(|line| {
            line.strip_prefix(label)
                .and_then(|value| value.strip_prefix('='))
                .map(str::to_owned)
        })
    }

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
                &parsed.calls[0],
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
            .find(|reference| {
                reference
                    .keys
                    .iter()
                    .any(|key| key == "rust:function:duplicate")
            })
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
    fn scopes_associated_types_to_their_impl_owner() {
        let sources = [
            Source {
                path: "src/lib.rs".into(),
                text: "mod model; use crate::model::Cursor; mod traits { pub trait Stream { type Item; fn next(&mut self); } } use crate::traits::Stream as Flow; impl Flow for Cursor { type Item = u8; fn next(&mut self) {} }".into(),
            },
            Source {
                path: "src/model.rs".into(),
                text: "pub struct Cursor;".into(),
            },
        ];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();

        for (item_key, owner_key) in [
            ("rust:type:traits::Stream::Item", "rust:type:traits::Stream"),
            ("rust:type:model::Cursor::Item", "rust:type:model::Cursor"),
        ] {
            let item = graph
                .nodes
                .iter()
                .find(|node| node.keys.iter().any(|key| key == item_key))
                .unwrap();
            let owner = graph
                .nodes
                .iter()
                .find(|node| node.keys.iter().any(|key| key == owner_key))
                .unwrap();
            assert_eq!(item.parent_key, Some(owner.key.clone()));
        }
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| !node.keys.contains(&"rust:type:Item".into()))
        );
        assert!(graph.trait_implementations.iter().any(|implementation| {
            implementation.implementor_key == "rust:item:model::Cursor"
                && implementation.trait_key == "rust:item:traits::Stream"
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
    fn resolves_typed_receivers_and_one_hop_public_reexports() {
        let sources = [Source {
            path: "src/lib.rs".into(),
            text: r#"
mod model {
    pub struct Worker;
    impl Worker { pub fn run(&self) {} }
    pub fn helper() {}
}
mod api {
    pub use crate::model::helper as execute;
    #[cfg(unix)] pub use crate::model::helper as maybe;
    #[cfg(windows)] pub use external::helper as maybe;
}
mod facade { pub use crate::api::execute as launch; }
use crate::model::Worker as Job;
use crate::api::{execute, maybe};
use crate::facade::launch;
fn call(job: Job) {
    job.run();
    let other: crate::model::Worker = job;
    other.run();
    let inferred = other;
    inferred.run();
    execute();
    maybe();
    launch();
}
"#
            .into(),
        }];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();
        let call = graph.nodes.iter().find(|node| node.name == "call").unwrap();
        let target = |name: &str| {
            graph
                .nodes
                .iter()
                .find(|node| node.name == name)
                .map(|node| node.key.as_str())
                .unwrap()
        };

        assert_eq!(
            graph
                .refs
                .iter()
                .filter(|reference| {
                    reference.source_key == call.key
                        && reference.resolved_target_key.as_deref() == Some(target("run"))
                })
                .count(),
            2
        );
        assert!(graph.refs.iter().any(|reference| {
            reference.source_key == call.key
                && reference.resolved_target_key.as_deref() == Some(target("helper"))
        }));
        assert!(graph.refs.iter().any(|reference| {
            reference.source_key == call.key
                && reference
                    .keys
                    .iter()
                    .any(|key| key == "rust:item:facade::launch")
                && reference.resolved_target_key.is_none()
        }));
        assert!(graph.refs.iter().any(|reference| {
            reference.source_key == call.key
                && reference
                    .keys
                    .iter()
                    .any(|key| key == "rust:item:api::maybe")
                && reference.resolved_target_key.is_none()
        }));
        assert!(!graph.refs.iter().any(|reference| {
            reference.source_key == call.key
                && reference
                    .keys
                    .iter()
                    .any(|key| key.contains("inferred::run"))
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
    fn resolves_one_module_glob_after_lexical_candidates() {
        let sources = [Source {
            path: "src/lib.rs".into(),
            text: r#"
pub fn verify_chain() {}
pub fn public_entry() {}
pub struct AuditRecord;
impl AuditRecord { pub fn build() {} }

#[cfg(test)]
mod tests {
    use super::*;
    fn verify_chain() {}
    #[test]
    fn checks_chain() {
        verify_chain();
        public_entry();
        AuditRecord::build();
    }
}
"#
            .into(),
        }];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();
        let node = |key: &str| {
            graph
                .nodes
                .iter()
                .find(|node| node.keys.iter().any(|candidate| candidate == key))
                .unwrap()
        };
        let test = node("rust:function:tests::checks_chain");

        for target in [
            node("rust:function:tests::verify_chain"),
            node("rust:function:public_entry"),
            node("rust:method:AuditRecord::build"),
        ] {
            assert!(graph.refs.iter().any(|reference| {
                reference.source_key == test.key
                    && reference.resolved_target_key.as_deref() == Some(target.key.as_str())
            }));
        }

        let outer = node("rust:function:verify_chain");
        assert!(!graph.refs.iter().any(|reference| {
            reference.source_key == test.key
                && reference.resolved_target_key.as_deref() == Some(outer.key.as_str())
        }));
    }

    #[test]
    fn local_types_shadow_glob_imported_qualified_calls() {
        let sources = [Source {
            path: "src/lib.rs".into(),
            text: r#"
pub struct AuditRecord;
impl AuditRecord { pub fn build() {} }

mod tests {
    use super::*;

    #[test]
    fn checks_local_type() {
        struct AuditRecord;
        impl AuditRecord { fn build() {} }
        AuditRecord::build();
    }
}
"#
            .into(),
        }];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();
        let node = |key: &str| {
            graph
                .nodes
                .iter()
                .find(|node| node.keys.iter().any(|candidate| candidate == key))
                .unwrap()
        };
        let test = node("rust:function:tests::checks_local_type");
        let local = node("rust:method:tests::checks_local_type::AuditRecord::build");
        let outer = node("rust:method:AuditRecord::build");
        let call = graph
            .refs
            .iter()
            .find(|reference| reference.source_key == test.key)
            .unwrap();

        assert_eq!(
            call.resolved_target_key.as_deref(),
            Some(local.key.as_str())
        );
        assert_ne!(
            call.resolved_target_key.as_deref(),
            Some(outer.key.as_str())
        );
    }

    #[test]
    fn nested_block_types_do_not_shadow_out_of_block_qualified_calls() {
        let sources = [Source {
            path: "src/lib.rs".into(),
            text: r#"
pub struct AuditRecord;
impl AuditRecord { pub fn build() {} }

mod tests {
    use super::*;

    #[test]
    fn checks_outer_type() {
        if false {
            struct AuditRecord;
            impl AuditRecord { fn build() {} }
        }
        AuditRecord::build();
    }
}
"#
            .into(),
        }];
        let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();
        let node = |key: &str| {
            graph
                .nodes
                .iter()
                .find(|node| node.keys.iter().any(|candidate| candidate == key))
                .unwrap()
        };
        let test = node("rust:function:tests::checks_outer_type");
        let nested = node("rust:method:tests::checks_outer_type::AuditRecord::build");
        let outer = node("rust:method:AuditRecord::build");
        let call = graph
            .refs
            .iter()
            .find(|reference| reference.source_key == test.key)
            .unwrap();

        assert_eq!(
            call.resolved_target_key.as_deref(),
            Some(outer.key.as_str())
        );
        assert_ne!(
            call.resolved_target_key.as_deref(),
            Some(nested.key.as_str())
        );
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

    #[test]
    fn target_layout_uses_captured_inventory_not_live_files() {
        let fixture = std::env::temp_dir().join(format!(
            "graphr-target-layout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(fixture.join("src")).unwrap();
        fs::write(
            fixture.join("Cargo.toml"),
            "[package]\nname='live'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(fixture.join("src/lib.rs"), "pub mod worker;\n").unwrap();

        let mut layout = TargetLayout::from_inventory(["src/worker.rs"], std::iter::empty());

        assert_eq!(
            layout.for_path("src/worker.rs").parse_context(),
            "22:@file:13:src/worker.rs@file:13:src/worker.rs"
        );
        fs::remove_dir_all(fixture).unwrap();
    }
}
