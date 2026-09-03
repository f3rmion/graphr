#[cfg(test)]
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::git::{
    CapturedSource, ChangeLayer, DependencyMode, Repository, SourceOmission, WorktreeChanges,
    resolve_commit,
};
use crate::index::IndexStats;
use crate::store;

pub use crate::index::Engine;

pub(crate) const CACHE_FORMAT_VERSION: u32 = 10;
pub(crate) const GRAPH_ANALYZER_VERSION: u32 = 7;
pub(crate) const REVIEW_FORMAT_VERSION: u32 = 6;
const MANIFEST_SIZE_LIMIT: u64 = 64 * 1024;
const REVIEW_SIZE_LIMIT: u64 = 64 * 1024 * 1024;
static PRIVATE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum ErrorCode {
    InvalidParameters,
    RootUnknown,
    RootDisallowed,
    RootStale,
    RootNotWorktree,
    GitMetadataInvalid,
    RefNotFound,
    HeadWorktreeMismatch,
    CaptureChanged,
    WorkspaceBusy,
    JobNotFound,
    JobCancelled,
    SnapshotNotFound,
    SnapshotIncomplete,
    CacheCorrupt,
    CursorSnapshotMismatch,
    CursorParametersMismatch,
    NodeSnapshotMismatch,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct OperationError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

impl OperationError {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: BTreeMap::new(),
        }
    }

    pub(crate) fn with_path(mut self, key: &str, path: &Path) -> Self {
        self.details.insert(key.into(), path.display().to_string());
        self
    }

    pub(crate) fn with_detail(mut self, key: &str, value: impl Into<String>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }
}

impl std::fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OperationError {}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RootIdentity {
    pub repository_id: String,
    pub workspace_id: String,
    pub repository_root: PathBuf,
    pub worktree_root: PathBuf,
    pub git_dir: PathBuf,
    pub common_git_dir: PathBuf,
    #[serde(skip)]
    #[schemars(skip)]
    pub common_git_dir_dev: u64,
    #[serde(skip)]
    #[schemars(skip)]
    pub common_git_dir_ino: u64,
    pub index_path: PathBuf,
    pub object_format: String,
    pub branch: Option<String>,
    pub head_oid: String,
}

#[derive(
    Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, rmcp::schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
pub enum SnapshotTarget {
    Commit,
    Index,
    Worktree { include_untracked: bool },
}

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    serde::Deserialize,
    serde::Serialize,
    rmcp::schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum NoChangeReason {
    IdenticalCommitOids,
    IdenticalTrees,
    EmptyIndexDelta,
    EmptyWorktreeDelta,
}

impl NoChangeReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdenticalCommitOids => "identical_commit_oids",
            Self::IdenticalTrees => "identical_trees",
            Self::EmptyIndexDelta => "empty_index_delta",
            Self::EmptyWorktreeDelta => "empty_worktree_delta",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RootInspection {
    pub identity: RootIdentity,
    pub staged_paths: usize,
    pub unstaged_paths: usize,
    pub untracked_paths: usize,
    pub snapshot_id: Option<String>,
    pub snapshot_matches_worktree: Option<bool>,
    pub changed_identity_fields: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexRequest {
    pub worktree_root: PathBuf,
    pub base_ref: String,
    pub head_ref: String,
    pub target: SnapshotTarget,
    pub dependency_mode: DependencyMode,
    pub evidence_manifest: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedIndexRequest {
    pub root: RootIdentity,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
    pub target: SnapshotTarget,
    pub dependency_mode: DependencyMode,
    pub evidence_manifest: Option<PathBuf>,
}

#[derive(
    Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, rmcp::schemars::JsonSchema,
)]
#[schemars(crate = "rmcp::schemars")]
pub struct Provenance {
    pub repository_id: String,
    pub workspace_id: String,
    pub snapshot_id: String,
    pub common_git_dir: PathBuf,
    pub git_dir: PathBuf,
    pub repository_root: PathBuf,
    pub worktree_root: PathBuf,
    pub branch: Option<String>,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
    pub target_state: SnapshotTarget,
    pub selected_layers: Vec<ChangeLayer>,
    pub dirty_digest: String,
    pub commits_base_to_head: u64,
    pub changed_files: usize,
    pub index_generation: i64,
    pub source_snapshot_id: Option<String>,
    pub evidence_manifest_digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildStage {
    Capturing,
    SelectingSeed,
    Indexing,
    ResolvingGraph,
    Publishing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildProgress {
    pub stage: BuildStage,
    pub files_done: usize,
    pub files_total: usize,
    pub files_reused: usize,
    pub files_parsed: usize,
    pub rejected_cache: Option<String>,
}

#[derive(
    Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, rmcp::schemars::JsonSchema,
)]
#[schemars(crate = "rmcp::schemars")]
pub struct IndexCompletion {
    pub snapshot_id: String,
    pub graph_image_id: String,
    pub provenance: Provenance,
    pub stats: IndexStats,
}

#[derive(Debug)]
pub struct QueryOutput {
    pub text: String,
    pub provenance: Provenance,
    pub no_change_reason: Option<NoChangeReason>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct SnapshotManifest {
    format_version: u32,
    graph_image_id: String,
    graph_checksum: String,
    review_id: String,
    review_format_version: u32,
    dependency_mode: DependencyMode,
    no_change_reason: Option<NoChangeReason>,
    provenance: Provenance,
}

#[derive(Debug)]
pub struct SnapshotEntry {
    pub graph_image_id: String,
    pub graph_path: PathBuf,
    pub changes: Arc<WorktreeChanges>,
    pub no_change_reason: Option<NoChangeReason>,
    pub dependency_mode: DependencyMode,
    pub provenance: Provenance,
    graph_file: Arc<File>,
    graph_checksum: String,
}

impl SnapshotEntry {
    /// Opens the graph image this entry validated. SQLite cannot take a
    /// descriptor, so the open is pinned to the descriptor retained at load
    /// time: renaming `graph_path` afterwards cannot redirect the read to
    /// another inode.
    pub(crate) fn open_graph(&self) -> std::result::Result<store::Store, String> {
        let pin = crate::pinned::pin(&self.graph_path, &self.graph_file)?;
        let store = store::Store::open_reader(&self.graph_path)?;
        pin.require_used()?;
        Ok(store)
    }
}

pub(crate) struct PinnedGraph {
    file: Arc<File>,
    checksum: String,
}

pub struct SnapshotCatalog {
    allowed_roots: Arc<AllowedRoots>,
    loaded: RwLock<HashMap<String, Arc<SnapshotEntry>>>,
    rejected: RwLock<HashMap<String, RejectedSnapshot>>,
}

#[derive(Clone)]
struct RejectedSnapshot {
    repository_id: String,
    cache: CachePaths,
    manifest_name: String,
    error: OperationError,
}

#[derive(Clone)]
pub struct AllowedRoots {
    roots: Vec<AllowedRoot>,
}

#[derive(Clone)]
struct AllowedRoot {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl AllowedRoots {
    pub fn new(paths: Vec<PathBuf>) -> Result<Self, OperationError> {
        if paths.is_empty() {
            return Err(OperationError::new(
                ErrorCode::InvalidParameters,
                "at least one allowed root is required",
            ));
        }

        let mut roots = Vec::with_capacity(paths.len());
        for path in paths {
            validate_path(&path, "allowed root")?;
            let path = fs::canonicalize(&path).map_err(|_| {
                OperationError::new(ErrorCode::RootUnknown, "allowed root does not exist")
                    .with_path("root", &path)
            })?;
            let metadata = fs::metadata(&path).map_err(|_| {
                OperationError::new(ErrorCode::RootUnknown, "cannot inspect allowed root")
                    .with_path("root", &path)
            })?;
            if !metadata.is_dir() {
                return Err(OperationError::new(
                    ErrorCode::InvalidParameters,
                    "allowed root is not a directory",
                )
                .with_path("root", &path));
            }
            roots.push(AllowedRoot {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
            });
        }
        roots.sort_by(|left, right| {
            left.path
                .components()
                .count()
                .cmp(&right.path.components().count())
                .then_with(|| left.path.cmp(&right.path))
        });
        roots.dedup_by(|left, right| left.path == right.path);
        let mut retained = Vec::with_capacity(roots.len());
        for root in roots {
            if !retained
                .iter()
                .any(|parent: &AllowedRoot| root.path.starts_with(&parent.path))
            {
                retained.push(root);
            }
        }
        Ok(Self { roots: retained })
    }

    pub fn inspect(
        &self,
        requested: &Path,
        cancelled: &AtomicBool,
    ) -> Result<RootIdentity, OperationError> {
        validate_path(requested, "requested root")?;
        let requested = fs::canonicalize(requested).map_err(|_| {
            OperationError::new(ErrorCode::RootUnknown, "requested root does not exist")
                .with_path("root", requested)
        })?;
        if !requested.is_dir() {
            return Err(OperationError::new(
                ErrorCode::RootUnknown,
                "requested root is not a directory",
            )
            .with_path("root", &requested));
        }
        self.authorize(&requested)?;
        let repository = Repository::discover_cancelled(&requested, cancelled)?;
        self.authorize(&repository.root)?;
        #[cfg(test)]
        after_repository_discovery(&repository);
        Ok(identity(repository))
    }

    pub fn authorize(&self, canonical_root: &Path) -> Result<(), OperationError> {
        let allowed = self
            .roots
            .iter()
            .find(|allowed| canonical_root.starts_with(&allowed.path))
            .ok_or_else(|| {
                OperationError::new(ErrorCode::RootDisallowed, "root is outside allowed roots")
                    .with_path("root", canonical_root)
            })?;
        let metadata = fs::metadata(&allowed.path).map_err(|_| {
            OperationError::new(ErrorCode::RootStale, "allowed root no longer exists")
                .with_path("root", &allowed.path)
        })?;
        if !metadata.is_dir() || metadata.dev() != allowed.device || metadata.ino() != allowed.inode
        {
            return Err(
                OperationError::new(ErrorCode::RootStale, "allowed root was replaced")
                    .with_path("root", &allowed.path),
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
struct CachePaths {
    graphs: CacheDirectory,
    reviews: CacheDirectory,
    snapshots: CacheDirectory,
    quarantine: CacheDirectory,
    tmp: CacheDirectory,
}

#[derive(Clone, Debug)]
struct CacheDirectory {
    path: PathBuf,
    handle: Option<Arc<File>>,
}

pub(crate) struct PrivateJob {
    root: RootIdentity,
    cache: CachePaths,
    directory: CacheDirectory,
    _capture_directory: CacheDirectory,
    name: String,
    capture_root: PathBuf,
    graph_temp: PathBuf,
}

impl PrivateJob {
    pub(crate) fn capture_root(&self) -> &Path {
        &self.capture_root
    }

    pub(crate) fn graph_temp(&self) -> &Path {
        &self.graph_temp
    }

    pub(crate) fn graph_path(&self, graph_image_id: &str) -> PathBuf {
        self.cache
            .graphs
            .child_path(OsStr::new(&format!("{graph_image_id}.db")))
    }

    pub(crate) fn copy_seed(
        &self,
        seed: &SnapshotEntry,
        cancelled: &AtomicBool,
    ) -> Result<(), OperationError> {
        #[cfg(test)]
        BEFORE_SEED_OPEN_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook(&seed.graph_path);
            }
        });
        let result = (|| {
            let mut target = create_file_at(&self.directory, OsStr::new("graph.db"), 0o600)
                .map_err(|error| cache_internal("cannot create private graph image", error))?;
            copy_descriptor(&seed.graph_file, &mut target)?;
            target
                .sync_all()
                .map_err(|error| cache_internal("cannot sync graph seed", error))?;
            let copied = open_regular(&self.graph_temp, None).map_err(cache_corrupt)?;
            if hash_file(&copied, cancelled)? != seed.graph_checksum {
                return Err(cache_corrupt("copied graph seed checksum is invalid"));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&self.graph_temp);
        }
        result
    }
}

impl Drop for PrivateJob {
    fn drop(&mut self) {
        if let Ok(entries) = read_dir_at(&self.directory) {
            for entry in entries {
                match stat_at(&self.directory, &entry) {
                    Ok(metadata) if metadata.st_mode & libc::S_IFMT == libc::S_IFDIR => {
                        let _ = remove_tree_at(&self.directory, &entry);
                    }
                    _ => {
                        let _ = unlink_at(&self.directory, &entry, 0);
                    }
                }
            }
        }
        let _ = unlink_at(&self.cache.tmp, OsStr::new(&self.name), libc::AT_REMOVEDIR);
        let _ = self.cache.tmp.sync();
    }
}

impl SnapshotCatalog {
    pub fn new(allowed_roots: Arc<AllowedRoots>) -> Self {
        Self {
            allowed_roots,
            loaded: RwLock::new(HashMap::new()),
            rejected: RwLock::new(HashMap::new()),
        }
    }

    pub fn attach(
        &self,
        root: &RootIdentity,
        cancelled: &AtomicBool,
    ) -> Result<(), OperationError> {
        check_cache_cancelled(cancelled)?;
        self.allowed_roots.authorize(&root.worktree_root)?;
        let Some(cache) = cache_paths(root, false)? else {
            self.reconcile(root, &BTreeSet::new());
            return Ok(());
        };
        let Some(_directory) = cache.snapshots.handle.as_ref() else {
            self.reconcile(root, &BTreeSet::new());
            return Ok(());
        };
        let mut manifests = read_dir_at(&cache.snapshots)
            .map_err(|error| cache_internal("cannot read snapshot catalog", error))?;
        manifests.sort();
        let mut seen = BTreeSet::new();
        for file_name in manifests {
            check_cache_cancelled(cancelled)?;
            let Some(snapshot_id) = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .filter(|id| valid_id(id))
            else {
                continue;
            };
            seen.insert(snapshot_id.to_owned());
            match self.load_entry(root, &cache, snapshot_id, &file_name, cancelled) {
                Ok(entry) => {
                    write_lock(&self.loaded).insert(snapshot_id.into(), entry);
                    write_lock(&self.rejected).remove(snapshot_id);
                }
                Err(error) if error.code == ErrorCode::JobCancelled => return Err(error),
                Err(error) => {
                    write_lock(&self.loaded).remove(snapshot_id);
                    write_lock(&self.rejected).insert(
                        snapshot_id.into(),
                        RejectedSnapshot {
                            repository_id: root.repository_id.clone(),
                            cache: cache.clone(),
                            manifest_name: file_name.to_string_lossy().into_owned(),
                            error,
                        },
                    );
                }
            }
        }
        self.reconcile(root, &seen);
        Ok(())
    }

    fn reconcile(&self, root: &RootIdentity, seen: &BTreeSet<String>) {
        write_lock(&self.loaded).retain(|snapshot_id, entry| {
            entry.provenance.repository_id != root.repository_id || seen.contains(snapshot_id)
        });
        write_lock(&self.rejected).retain(|snapshot_id, rejected| {
            rejected.repository_id != root.repository_id || seen.contains(snapshot_id)
        });
    }

    pub fn get(&self, snapshot_id: &str) -> Result<Arc<SnapshotEntry>, OperationError> {
        if !valid_id(snapshot_id) {
            return Err(snapshot_not_found(snapshot_id));
        }
        if let Some(entry) = read_lock(&self.loaded).get(snapshot_id) {
            return Ok(entry.clone());
        }
        if let Some(rejected) = read_lock(&self.rejected).get(snapshot_id) {
            return Err(rejected.error.clone());
        }
        Err(snapshot_not_found(snapshot_id))
    }

    pub(crate) fn entries(&self, repository_id: &str) -> Vec<Arc<SnapshotEntry>> {
        let mut entries = read_lock(&self.loaded)
            .values()
            .filter(|entry| entry.provenance.repository_id == repository_id)
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.provenance
                .snapshot_id
                .cmp(&right.provenance.snapshot_id)
        });
        entries
    }

    pub(crate) fn pin_exact_graph(
        &self,
        job: &PrivateJob,
        graph_image_id: &str,
        trusted: Option<&SnapshotEntry>,
        cancelled: &AtomicBool,
    ) -> Result<Option<PinnedGraph>, OperationError> {
        let name = format!("{graph_image_id}.db");
        let current = match open_regular_at(&job.cache.graphs, OsStr::new(&name), None) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(cache_corrupt(error)),
            Ok(file) => Arc::new(file),
        };
        let (file, checksum) = match trusted {
            Some(trusted) => {
                if !same_file(&current, &trusted.graph_file)? {
                    return Err(cache_corrupt(
                        "published graph name changed after exact validation",
                    ));
                }
                validate_entry_graph(trusted, cancelled)?;
                (trusted.graph_file.clone(), trusted.graph_checksum.clone())
            }
            None => {
                let path = job.cache.graphs.child_path(OsStr::new(&name));
                validate_published_image(&path)?;
                let checksum = hash_file(&current, cancelled)?;
                (current, checksum)
            }
        };
        Ok(Some(PinnedGraph { file, checksum }))
    }

    pub(crate) fn begin(&self, root: &RootIdentity) -> Result<PrivateJob, OperationError> {
        self.allowed_roots.authorize(&root.worktree_root)?;
        let cache = cache_paths(root, true)?.expect("created above");
        let path = loop {
            let name = private_name("job");
            match create_child_directory(&cache.tmp, OsStr::new(&name), true) {
                Ok(directory) => break (name, directory),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(cache_internal("cannot create private job directory", error));
                }
            }
        };
        cache.tmp.sync()?;
        let (name, directory) = path;
        let capture = create_child_directory(&directory, OsStr::new("capture"), false)
            .map_err(|error| cache_internal("cannot create private capture directory", error))?;
        let capture_root = capture.path.clone();
        Ok(PrivateJob {
            root: root.clone(),
            cache,
            graph_temp: directory.child_path(OsStr::new("graph.db")),
            directory,
            _capture_directory: capture,
            name,
            capture_root,
        })
    }

    pub(crate) fn prepare_publication(
        &self,
        job: &PrivateJob,
        snapshot_id: &str,
        review_id: &str,
        expected_review: &[u8],
    ) -> Result<Option<String>, OperationError> {
        let mut rejected_path = None;
        let rejected = { read_lock(&self.rejected).get(snapshot_id).cloned() };
        if let Some(rejected) = rejected {
            if !matches!(
                rejected.error.code,
                ErrorCode::CacheCorrupt | ErrorCode::SnapshotIncomplete
            ) {
                return Err(rejected.error);
            }
            quarantine_name(
                &job.cache,
                &rejected.cache.snapshots,
                OsStr::new(&rejected.manifest_name),
            )?;
            rejected_path = Some(
                rejected
                    .cache
                    .snapshots
                    .path
                    .join(&rejected.manifest_name)
                    .display()
                    .to_string(),
            );
            write_lock(&self.loaded).remove(snapshot_id);
            write_lock(&self.rejected).remove(snapshot_id);
        }

        let review_name = format!("{review_id}.json");
        match read_bounded_at(
            &job.cache.reviews,
            OsStr::new(&review_name),
            REVIEW_SIZE_LIMIT,
        ) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(bytes)
                if bytes == expected_review
                    && blake3::hash(&bytes).to_hex().as_str() == review_id => {}
            Ok(_) | Err(_) => {
                quarantine_name(&job.cache, &job.cache.reviews, OsStr::new(&review_name))?;
                rejected_path = Some(
                    job.cache
                        .reviews
                        .path
                        .join(review_name)
                        .display()
                        .to_string(),
                );
            }
        }
        Ok(rejected_path)
    }

    pub(crate) fn quarantine_rejected(
        &self,
        root: &RootIdentity,
        requested_snapshot_id: &str,
    ) -> Result<Option<String>, OperationError> {
        let cache = cache_paths(root, true)?.expect("created above");
        let rejected = {
            read_lock(&self.rejected)
                .iter()
                .filter(|(snapshot_id, rejected)| {
                    snapshot_id.as_str() != requested_snapshot_id
                        && rejected.repository_id == root.repository_id
                        && matches!(
                            rejected.error.code,
                            ErrorCode::CacheCorrupt | ErrorCode::SnapshotIncomplete
                        )
                })
                .map(|(snapshot_id, rejected)| (snapshot_id.clone(), rejected.clone()))
                .collect::<Vec<_>>()
        };
        let mut rejected_path = None;
        for (snapshot_id, rejected) in rejected {
            quarantine_name(
                &cache,
                &rejected.cache.snapshots,
                OsStr::new(&rejected.manifest_name),
            )?;
            rejected_path = Some(
                rejected
                    .cache
                    .snapshots
                    .path
                    .join(&rejected.manifest_name)
                    .display()
                    .to_string(),
            );
            write_lock(&self.loaded).remove(&snapshot_id);
            write_lock(&self.rejected).remove(&snapshot_id);
        }
        Ok(rejected_path)
    }

    #[allow(clippy::too_many_arguments)] // One call publishes one complete immutable snapshot.
    pub(crate) fn publish(
        &self,
        job: &PrivateJob,
        graph_image_id: &str,
        review_id: &str,
        review_bytes: &[u8],
        graph_temp: Option<&Path>,
        trusted_graph: Option<&PinnedGraph>,
        dependency_mode: DependencyMode,
        no_change_reason: Option<NoChangeReason>,
        mut provenance: Provenance,
        cancelled: &AtomicBool,
    ) -> Result<Arc<SnapshotEntry>, OperationError> {
        if !valid_id(graph_image_id) || !valid_id(review_id) || !valid_id(&provenance.snapshot_id) {
            return Err(OperationError::new(
                ErrorCode::Internal,
                "generated cache identifier is invalid",
            ));
        }
        if review_bytes.len() as u64 > REVIEW_SIZE_LIMIT {
            return Err(OperationError::new(
                ErrorCode::Internal,
                "generated review exceeds the cache size limit",
            ));
        }
        write_private(&job.directory, OsStr::new("review.json"), review_bytes)?;
        let review_name = format!("{review_id}.json");
        #[cfg(test)]
        before_review_publication(&job.cache.reviews.path.join(&review_name))?;
        publish_no_replace(
            &job.directory,
            OsStr::new("review.json"),
            &job.cache.reviews,
            OsStr::new(&review_name),
        )?;
        let winner = read_bounded_at(
            &job.cache.reviews,
            OsStr::new(&review_name),
            REVIEW_SIZE_LIMIT,
        )
        .map_err(cache_corrupt)?;
        if blake3::hash(&winner).to_hex().as_str() != review_id {
            return Err(cache_corrupt(
                "published review checksum does not match its ID",
            ));
        }

        let graph_name = format!("{graph_image_id}.db");
        if let Some(graph_temp) = graph_temp {
            fs::set_permissions(graph_temp, fs::Permissions::from_mode(0o444))
                .map_err(|error| cache_internal("cannot make graph image read-only", error))?;
            open_regular(graph_temp, None)
                .and_then(|file| file.sync_all())
                .map_err(|error| cache_internal("cannot sync read-only graph image", error))?;
            let _ = graph_temp;
            publish_no_replace(
                &job.directory,
                OsStr::new("graph.db"),
                &job.cache.graphs,
                OsStr::new(&graph_name),
            )?;
        }
        let graph_file = open_regular_at(&job.cache.graphs, OsStr::new(&graph_name), None)
            .map_err(cache_corrupt)?;
        if let Some(trusted) = trusted_graph
            && !same_file(&graph_file, &trusted.file)?
        {
            return Err(cache_corrupt(
                "published graph name changed after exact validation",
            ));
        }
        let graph_path = job.cache.graphs.child_path(OsStr::new(&graph_name));
        let (state, graph_checksum) = match trusted_graph {
            Some(trusted) => (
                validate_pinned_graph(trusted, &graph_path, cancelled)?,
                trusted.checksum.clone(),
            ),
            None => (
                validate_published_image(&graph_path)?,
                hash_file(&graph_file, cancelled)?,
            ),
        };
        provenance.index_generation = state.generation;
        let manifest = SnapshotManifest {
            format_version: CACHE_FORMAT_VERSION,
            graph_image_id: graph_image_id.into(),
            graph_checksum,
            review_id: review_id.into(),
            review_format_version: REVIEW_FORMAT_VERSION,
            dependency_mode,
            no_change_reason,
            provenance,
        };
        let manifest_bytes = rmcp::serde_json::to_vec(&manifest).map_err(|error| {
            OperationError::new(
                ErrorCode::Internal,
                format!("cannot serialize snapshot: {error}"),
            )
        })?;
        if manifest_bytes.len() as u64 > MANIFEST_SIZE_LIMIT {
            return Err(OperationError::new(
                ErrorCode::Internal,
                "generated manifest exceeds the cache size limit",
            ));
        }
        write_private(&job.directory, OsStr::new("snapshot.json"), &manifest_bytes)?;
        let manifest_name = format!("{}.json", manifest.provenance.snapshot_id);
        #[cfg(test)]
        before_manifest_publication(&PublicationPoint {
            snapshot_id: manifest.provenance.snapshot_id.clone(),
            graph_path: graph_path.clone(),
            review_path: job.cache.reviews.child_path(OsStr::new(&review_name)),
            manifest_path: job.cache.snapshots.child_path(OsStr::new(&manifest_name)),
        })?;
        publish_no_replace(
            &job.directory,
            OsStr::new("snapshot.json"),
            &job.cache.snapshots,
            OsStr::new(&manifest_name),
        )?;
        // The manifest is the publication point, so cancellation has lost the race.
        let entry = self.load_entry(
            &job.root,
            &job.cache,
            &manifest.provenance.snapshot_id,
            OsStr::new(&manifest_name),
            &AtomicBool::new(false),
        )?;
        write_lock(&self.loaded).insert(manifest.provenance.snapshot_id.clone(), entry.clone());
        write_lock(&self.rejected).remove(&manifest.provenance.snapshot_id);
        Ok(entry)
    }

    pub(crate) fn quarantine_graph(
        &self,
        root: &RootIdentity,
        graph_image_id: &str,
        requested_snapshot_id: &str,
    ) -> Result<(), OperationError> {
        let cache = cache_paths(root, true)?.expect("created above");
        let graph_name = format!("{graph_image_id}.db");
        quarantine_name(&cache, &cache.graphs, OsStr::new(&graph_name))?;
        let mut snapshot_ids = BTreeSet::from([requested_snapshot_id.to_owned()]);
        for entry in read_lock(&self.loaded).values() {
            if entry.graph_image_id == graph_image_id {
                snapshot_ids.insert(entry.provenance.snapshot_id.clone());
            }
        }
        for snapshot_id in &snapshot_ids {
            quarantine_name(
                &cache,
                &cache.snapshots,
                OsStr::new(&format!("{snapshot_id}.json")),
            )?;
        }
        write_lock(&self.loaded).retain(|_, entry| entry.graph_image_id != graph_image_id);
        let mut rejected = write_lock(&self.rejected);
        for snapshot_id in snapshot_ids {
            rejected.remove(&snapshot_id);
        }
        Ok(())
    }

    fn load_entry(
        &self,
        attached_root: &RootIdentity,
        cache: &CachePaths,
        snapshot_id: &str,
        manifest_name: &OsStr,
        cancelled: &AtomicBool,
    ) -> Result<Arc<SnapshotEntry>, OperationError> {
        check_cache_cancelled(cancelled)?;
        let bytes = match read_bounded_at(&cache.snapshots, manifest_name, MANIFEST_SIZE_LIMIT) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(snapshot_not_found(snapshot_id));
            }
            result => result.map_err(cache_corrupt)?,
        };
        let manifest: SnapshotManifest = rmcp::serde_json::from_slice(&bytes)
            .map_err(|_| cache_corrupt("snapshot manifest is invalid"))?;
        if manifest.format_version != CACHE_FORMAT_VERSION
            || manifest.review_format_version != REVIEW_FORMAT_VERSION
            || !valid_id(snapshot_id)
            || !valid_id(&manifest.graph_image_id)
            || !valid_id(&manifest.graph_checksum)
            || !valid_id(&manifest.review_id)
            || !valid_id(&manifest.provenance.repository_id)
            || !valid_id(&manifest.provenance.workspace_id)
            || !valid_id(&manifest.provenance.snapshot_id)
            || !valid_id(&manifest.provenance.dirty_digest)
            || manifest
                .provenance
                .source_snapshot_id
                .as_deref()
                .is_some_and(|id| !valid_id(id))
            || manifest
                .provenance
                .evidence_manifest_digest
                .as_deref()
                .is_some_and(|id| !valid_id(id))
            || manifest.provenance.source_snapshot_id.is_some()
                != manifest.provenance.evidence_manifest_digest.is_some()
            || manifest.provenance.snapshot_id != snapshot_id
            || !valid_git_oid(&manifest.provenance.base_oid)
            || !valid_git_oid(&manifest.provenance.head_oid)
        {
            return Err(cache_corrupt("snapshot manifest identity is invalid"));
        }
        if manifest.provenance.repository_id != attached_root.repository_id
            || manifest.provenance.common_git_dir != attached_root.common_git_dir
        {
            return Err(cache_corrupt("snapshot belongs to another repository"));
        }
        let authorized = self
            .allowed_roots
            .inspect(&manifest.provenance.worktree_root, cancelled)?;
        if authorized.repository_id != manifest.provenance.repository_id
            || authorized.workspace_id != manifest.provenance.workspace_id
            || authorized.common_git_dir != manifest.provenance.common_git_dir
            || authorized.git_dir != manifest.provenance.git_dir
            || authorized.repository_root != manifest.provenance.repository_root
            || authorized.worktree_root != manifest.provenance.worktree_root
        {
            return Err(cache_corrupt("snapshot root identity changed"));
        }

        let review_name = format!("{}.json", manifest.review_id);
        let review_bytes =
            match read_bounded_at(&cache.reviews, OsStr::new(&review_name), REVIEW_SIZE_LIMIT) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(OperationError::new(
                        ErrorCode::SnapshotIncomplete,
                        "snapshot review is not published",
                    ));
                }
                result => result.map_err(cache_corrupt)?,
            };
        check_cache_cancelled(cancelled)?;
        if blake3::hash(&review_bytes).to_hex().as_str() != manifest.review_id {
            return Err(cache_corrupt("snapshot review checksum is invalid"));
        }
        let changes: WorktreeChanges = rmcp::serde_json::from_slice(&review_bytes)
            .map_err(|_| cache_corrupt("snapshot review is invalid"))?;
        if selected_layers(&changes) != manifest.provenance.selected_layers
            || changed_file_count(&changes) != manifest.provenance.changed_files
            || manifest.no_change_reason
                != if manifest.provenance.evidence_manifest_digest.is_some() {
                    None
                } else {
                    expected_no_change_reason(
                        &changes,
                        &manifest.provenance.target_state,
                        &manifest.provenance.base_oid,
                        &manifest.provenance.head_oid,
                    )
                }
        {
            return Err(cache_corrupt("snapshot review provenance is invalid"));
        }
        let recomputed = snapshot_key(
            &SnapshotKeyInput {
                graph_image_id: &manifest.graph_image_id,
                workspace_id: &manifest.provenance.workspace_id,
                base_oid: &manifest.provenance.base_oid,
                head_oid: &manifest.provenance.head_oid,
                target: &manifest.provenance.target_state,
                dependency_mode: manifest.dependency_mode,
                dirty_digest: &manifest.provenance.dirty_digest,
                review_id: &manifest.review_id,
                source_snapshot_id: manifest.provenance.source_snapshot_id.as_deref(),
                evidence_manifest_digest: manifest.provenance.evidence_manifest_digest.as_deref(),
            },
            CACHE_FORMAT_VERSION,
            REVIEW_FORMAT_VERSION,
        );
        if recomputed != snapshot_id {
            return Err(cache_corrupt("snapshot ID does not match its manifest"));
        }

        #[cfg(test)]
        before_graph_load();
        if cache.graphs.handle.is_none() {
            return Err(OperationError::new(
                ErrorCode::SnapshotIncomplete,
                "snapshot graph is not published",
            ));
        }
        let graph_name = format!("{}.db", manifest.graph_image_id);
        let graph_file = match open_regular_at(&cache.graphs, OsStr::new(&graph_name), None) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(OperationError::new(
                    ErrorCode::SnapshotIncomplete,
                    "snapshot graph is not published",
                ));
            }
            Err(error) => return Err(cache_corrupt(error)),
            Ok(file) => file,
        };
        let graph_path = cache.graphs.child_path(OsStr::new(&graph_name));
        if hash_file(&graph_file, cancelled)? != manifest.graph_checksum
            || validate_published_image(&graph_path)?.generation
                != manifest.provenance.index_generation
        {
            return Err(cache_corrupt("snapshot graph checksum or state is invalid"));
        }
        Ok(Arc::new(SnapshotEntry {
            graph_image_id: manifest.graph_image_id,
            graph_path,
            changes: Arc::new(changes),
            no_change_reason: manifest.no_change_reason,
            dependency_mode: manifest.dependency_mode,
            provenance: manifest.provenance,
            graph_file: Arc::new(graph_file),
            graph_checksum: manifest.graph_checksum,
        }))
    }
}

/// Resolves the cache directories for a root, creating them when asked.
///
/// The canonicalisation clause below is load-bearing beyond this function:
/// `store` sends `SQLITE_OPEN_NOFOLLOW` unconditionally, and that flag rejects
/// a symlinked *ancestor* rather than only a symlinked final component. A
/// `common_git_dir` that is not already its own canonicalisation would
/// therefore be accepted here and then fail every database open deep inside
/// SQLite. `cache_paths_rejects_a_non_canonical_common_git_dir` pins it.
fn cache_paths(root: &RootIdentity, create: bool) -> Result<Option<CachePaths>, OperationError> {
    let metadata = fs::symlink_metadata(&root.common_git_dir)
        .map_err(|error| cache_internal("cannot inspect common Git directory", error))?;
    let canonical = fs::canonicalize(&root.common_git_dir)
        .map_err(|error| cache_internal("cannot resolve common Git directory", error))?;
    if !metadata.is_dir()
        || canonical != root.common_git_dir
        || !root.git_dir.starts_with(&root.common_git_dir)
    {
        return Err(OperationError::new(
            ErrorCode::GitMetadataInvalid,
            "common Git directory is not a validated directory",
        ));
    }
    let common = CacheDirectory::open_root(&root.common_git_dir, &metadata)
        .map_err(|error| cache_internal("cannot open common Git directory", error))?;
    let graphr = cache_child(&common, OsStr::new("graphr"), create, false)?;
    if graphr.handle.is_none() {
        return Ok(None);
    }
    let v6 = cache_child(&graphr, OsStr::new("v6"), create, false)?;
    if v6.handle.is_none() {
        return Ok(None);
    }
    Ok(Some(CachePaths {
        graphs: cache_child(&v6, OsStr::new("graphs"), create, false)?,
        reviews: cache_child(&v6, OsStr::new("reviews"), create, false)?,
        snapshots: cache_child(&v6, OsStr::new("snapshots"), create, false)?,
        quarantine: cache_child(&v6, OsStr::new("quarantine"), create, false)?,
        tmp: cache_child(&v6, OsStr::new("tmp"), create, false)?,
    }))
}

impl CacheDirectory {
    fn open_root(path: &Path, expected: &fs::Metadata) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)?;
        let actual = file.metadata()?;
        if !actual.is_dir() || actual.dev() != expected.dev() || actual.ino() != expected.ino() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cache root changed while opening",
            ));
        }
        Ok(Self {
            path: path.to_owned(),
            handle: Some(Arc::new(file)),
        })
    }

    /// Real filesystem path of a child. Callers that can act through a
    /// descriptor should use the primitives below instead; this exists for the
    /// one consumer that cannot take a descriptor, SQLite.
    fn child_path(&self, name: &OsStr) -> PathBuf {
        self.path.join(name)
    }

    fn file(&self) -> std::io::Result<&File> {
        self.handle.as_deref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "cache directory is absent")
        })
    }

    fn sync(&self) -> Result<(), OperationError> {
        self.file()
            .and_then(File::sync_all)
            .map_err(|error| cache_internal("cannot sync cache directory", error))
    }
}

// Portability seam. Every filesystem operation the cache performs goes through
// this block, and each one takes a pinned directory descriptor plus a single
// path component — never a composite path. `component_cstring` enforces the
// single-component rule.
//
// This shape is what makes the cache portable. A Windows backend would replace
// exactly these functions and nothing else: `openat` maps to `NtCreateFile`
// with `OBJECT_ATTRIBUTES.RootDirectory`, `linkat` to `NtSetInformationFile`
// with `FILE_LINK_INFORMATION`, `renameat` to `SetFileInformationByHandle` with
// `FILE_RENAME_INFO`.
//
// Do not add a path-taking helper here, and do not reconstruct a path from a
// descriptor. That is what tied the cache to Linux `/proc/self/fd`.
fn cache_child(
    parent: &CacheDirectory,
    name: &OsStr,
    create: bool,
    exclusive: bool,
) -> Result<CacheDirectory, OperationError> {
    let result = if create {
        create_child_directory(parent, name, exclusive)
    } else {
        open_child_directory(parent, name)
    };
    match result {
        Ok(handle) => Ok(handle),
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
            Ok(CacheDirectory {
                path: parent.path.join(name),
                handle: None,
            })
        }
        Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => Err(
            OperationError::new(ErrorCode::CacheCorrupt, "cache path is not a directory")
                .with_path("path", &parent.path.join(name)),
        ),
        Err(error) => Err(cache_internal("cannot open cache directory", error)),
    }
}

fn create_child_directory(
    parent: &CacheDirectory,
    name: &OsStr,
    exclusive: bool,
) -> std::io::Result<CacheDirectory> {
    let name_c = component_cstring(name)?;
    // SAFETY: the parent descriptor and NUL-terminated component remain valid for the call.
    let created = unsafe { libc::mkdirat(parent.file()?.as_raw_fd(), name_c.as_ptr(), 0o700) };
    if created == 0 {
        parent.file()?.sync_all()?;
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists || exclusive {
            return Err(error);
        }
    }
    open_child_directory(parent, name)
}

fn open_child_directory(parent: &CacheDirectory, name: &OsStr) -> std::io::Result<CacheDirectory> {
    let name_c = component_cstring(name)?;
    // SAFETY: openat receives a live directory descriptor and a valid C string.
    let fd = unsafe {
        libc::openat(
            parent.file()?.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a successful openat returns a newly owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    Ok(CacheDirectory {
        path: parent.path.join(name),
        handle: Some(Arc::new(file)),
    })
}

fn component_cstring(name: &OsStr) -> std::io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache name is not one path component",
        ));
    }
    CString::new(bytes).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache name contains NUL")
    })
}

fn create_file_at(directory: &CacheDirectory, name: &OsStr, mode: u32) -> std::io::Result<File> {
    let name = component_cstring(name)?;
    // SAFETY: openat receives a live directory descriptor and a valid C string.
    let fd = unsafe {
        libc::openat(
            directory.file()?.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: a successful openat returns a newly owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_regular_at(
    directory: &CacheDirectory,
    name: &OsStr,
    limit: Option<u64>,
) -> std::io::Result<File> {
    let name = component_cstring(name)?;
    // SAFETY: openat receives a live directory descriptor and a valid C string.
    let fd = unsafe {
        libc::openat(
            directory.file()?.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a successful openat returns a newly owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || limit.is_some_and(|limit| metadata.len() > limit) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cache file is not a bounded regular file",
        ));
    }
    Ok(file)
}

/// Owns a `DIR*` and the descriptor `fdopendir` took over. Deliberately exposes
/// no descriptor: after `fdopendir` the descriptor belongs to the stream, and
/// touching it separately — including closing it — is undefined behaviour.
struct DirStream(*mut libc::DIR);

impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful fdopendir, is never copied
        // out of this owner, and is closed exactly once.
        unsafe { libc::closedir(self.0) };
    }
}

/// Lists the names in a pinned directory, `.` and `..` excluded.
///
/// The stream runs on an independent description obtained with `openat(".")`,
/// never on the long-lived `CacheDirectory` handle: `closedir` would close that
/// handle out from under every other user of it, and `dup` would share one
/// directory position with them. `readdir_r` reports failure as a return code,
/// so end-of-directory never has to be told apart from an error by inspecting
/// `errno`, whose accessor is spelled differently per platform.
fn read_dir_at(directory: &CacheDirectory) -> std::io::Result<Vec<OsString>> {
    let dot = component_cstring(OsStr::new("."))?;
    // SAFETY: openat receives a live directory descriptor and a valid C string.
    let fd = unsafe {
        libc::openat(
            directory.file()?.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd is a freshly opened directory descriptor. On success fdopendir
    // takes ownership of it; on failure ownership stays here, so this is the
    // only path that closes it directly.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        // SAFETY: ownership did not transfer, and fd is closed exactly once.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let stream = DirStream(stream);
    let mut names = Vec::new();
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::dirent>::uninit();
        let mut current = std::ptr::null_mut();
        // SAFETY: the stream is live, and the entry buffer is a whole dirent,
        // which is what this platform's readdir_r fills.
        let code = unsafe { libc::readdir_r(stream.0, entry.as_mut_ptr(), &mut current) };
        if code != 0 {
            return Err(std::io::Error::from_raw_os_error(code));
        }
        if current.is_null() {
            return Ok(names);
        }
        // SAFETY: readdir_r reported an entry, so it initialized the buffer and
        // pointed current at it. d_name is NUL-terminated within the struct.
        let name = unsafe { CStr::from_ptr((*current).d_name.as_ptr()) };
        let name = OsStr::from_bytes(name.to_bytes());
        if name != OsStr::new(".") && name != OsStr::new("..") {
            names.push(name.to_owned());
        }
    }
}

fn link_at(
    source_directory: &CacheDirectory,
    source_name: &OsStr,
    target_directory: &CacheDirectory,
    target_name: &OsStr,
) -> std::io::Result<()> {
    let source = component_cstring(source_name)?;
    let target = component_cstring(target_name)?;
    let source_fd = source_directory.file()?.as_raw_fd();
    let target_fd = target_directory.file()?.as_raw_fd();
    let link = |flags| {
        // SAFETY: both descriptors stay live for the call and both C strings
        // are NUL-terminated single components.
        unsafe {
            libc::linkat(
                source_fd,
                source.as_ptr(),
                target_fd,
                target.as_ptr(),
                flags,
            )
        }
    };
    if link(0) == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOTSUP) {
        return Err(error);
    }
    // Some macOS filesystems reject linkat with flags = 0. Retrying with
    // AT_SYMLINK_FOLLOW is safe here because publish_no_replace opened
    // source_name with O_NOFOLLOW and confirmed it is a regular file, so it is
    // not a symlink and following cannot redirect the link.
    if link(libc::AT_SYMLINK_FOLLOW) == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn stat_at(directory: &CacheDirectory, name: &OsStr) -> std::io::Result<libc::stat> {
    let name = component_cstring(name)?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstatat receives a live directory descriptor and a valid C string.
    let result = unsafe {
        libc::fstatat(
            directory.file()?.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        // SAFETY: fstatat returned success, so metadata is initialized.
        Ok(unsafe { metadata.assume_init() })
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn stat_fd(file: &File) -> std::io::Result<libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat receives a live descriptor and an out pointer it fills.
    let result = unsafe { libc::fstat(file.as_raw_fd(), metadata.as_mut_ptr()) };
    if result == 0 {
        // SAFETY: fstat returned success, so metadata is initialized.
        Ok(unsafe { metadata.assume_init() })
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn unlink_at(directory: &CacheDirectory, name: &OsStr, flags: i32) -> std::io::Result<()> {
    let name = component_cstring(name)?;
    // SAFETY: unlinkat receives a live directory descriptor and a valid C string.
    let result = unsafe { libc::unlinkat(directory.file()?.as_raw_fd(), name.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Removes `name` and everything beneath it, descending through directory
/// descriptors so no composite path is ever handed to the kernel.
fn remove_tree_at(parent: &CacheDirectory, name: &OsStr) -> std::io::Result<()> {
    let directory = open_child_directory(parent, name)?;
    for entry in read_dir_at(&directory)? {
        if stat_at(&directory, &entry)?.st_mode & libc::S_IFMT == libc::S_IFDIR {
            remove_tree_at(&directory, &entry)?;
        } else {
            unlink_at(&directory, &entry, 0)?;
        }
    }
    unlink_at(parent, name, libc::AT_REMOVEDIR)
}

fn rename_at(
    source_directory: &CacheDirectory,
    source_name: &OsStr,
    target_directory: &CacheDirectory,
    target_name: &OsStr,
) -> std::io::Result<()> {
    let source_name = component_cstring(source_name)?;
    let target_name = component_cstring(target_name)?;
    // SAFETY: both directory descriptors and C strings remain valid for renameat.
    let result = unsafe {
        libc::renameat(
            source_directory.file()?.as_raw_fd(),
            source_name.as_ptr(),
            target_directory.file()?.as_raw_fd(),
            target_name.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn entry_exists_at(directory: &CacheDirectory, name: &OsStr) -> std::io::Result<bool> {
    let name = component_cstring(name)?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstatat initializes metadata on success; it is never read here.
    let result = unsafe {
        libc::fstatat(
            directory.file()?.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(true)
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

fn private_name(label: &str) -> String {
    format!(
        "{label}-{}-{}",
        std::process::id(),
        PRIVATE_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn write_private(
    directory: &CacheDirectory,
    name: &OsStr,
    bytes: &[u8],
) -> Result<(), OperationError> {
    let mut file = create_file_at(directory, name, 0o600)
        .map_err(|error| cache_internal("cannot create private cache file", error))?;
    file.write_all(bytes)
        .map_err(|error| cache_internal("cannot write private cache file", error))?;
    file.sync_all()
        .map_err(|error| cache_internal("cannot sync private cache file", error))
}

fn publish_no_replace(
    source_directory: &CacheDirectory,
    source_name: &OsStr,
    target_directory: &CacheDirectory,
    target_name: &OsStr,
) -> Result<bool, OperationError> {
    let source = open_regular_at(source_directory, source_name, None)
        .map_err(|error| cache_internal("cannot inspect private cache file", error))?;
    let metadata = source
        .metadata()
        .map_err(|error| cache_internal("cannot inspect private cache file", error))?;
    if !metadata.is_file() {
        return Err(OperationError::new(
            ErrorCode::Internal,
            "private cache path is not a regular file",
        ));
    }
    let published = match link_at(source_directory, source_name, target_directory, target_name) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(cache_internal("cannot publish cache file", error)),
    };
    if published {
        // linkat resolved source_name a second time. Confirm it reached the
        // inode validated above rather than one substituted in between.
        let linked = stat_at(target_directory, target_name)
            .map_err(|error| cache_internal("cannot inspect published cache file", error))?;
        // Compare raw stat fields: dev_t is i32 on macOS and u64 on Linux, so
        // a Rust-level comparison would need a platform-dependent cast.
        let validated = stat_fd(&source)
            .map_err(|error| cache_internal("cannot inspect private cache file", error))?;
        if linked.st_ino != validated.st_ino || linked.st_dev != validated.st_dev {
            let _ = unlink_at(target_directory, target_name, 0);
            return Err(OperationError::new(
                ErrorCode::Internal,
                "published cache file is not the validated file",
            ));
        }
    }
    unlink_at(source_directory, source_name, 0)
        .map_err(|error| cache_internal("cannot remove private cache name", error))?;
    source_directory.sync()?;
    target_directory.sync()?;
    Ok(published)
}

fn quarantine_name(
    cache: &CachePaths,
    source: &CacheDirectory,
    name: &OsStr,
) -> Result<(), OperationError> {
    if !entry_exists_at(source, name).map_err(cache_corrupt)? {
        return Ok(());
    }
    let display_name = name.to_string_lossy();
    let target = loop {
        let target = format!("{display_name}.{}", private_name("rejected"));
        if !entry_exists_at(&cache.quarantine, OsStr::new(&target)).map_err(cache_corrupt)? {
            break target;
        }
    };
    rename_at(source, name, &cache.quarantine, OsStr::new(&target))
        .map_err(|error| cache_corrupt(format!("cannot quarantine cache file: {error}")))?;
    source.sync()?;
    cache.quarantine.sync()
}

fn open_regular(path: &Path, limit: Option<u64>) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || limit.is_some_and(|limit| metadata.len() > limit) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cache file is not a bounded regular file",
        ));
    }
    Ok(file)
}

fn read_bounded_at(
    directory: &CacheDirectory,
    name: &OsStr,
    limit: u64,
) -> std::io::Result<Vec<u8>> {
    let mut file = open_regular_at(directory, name, Some(limit))?;
    let size = usize::try_from(file.metadata()?.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "cache file is too large")
    })?;
    read_exact_bounded(&mut file, size, limit)
}

fn read_exact_bounded(
    reader: &mut impl Read,
    expected_size: usize,
    limit: u64,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(expected_size);
    reader.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cache file grew beyond its size limit",
        ));
    }
    if bytes.len() != expected_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cache file changed while reading",
        ));
    }
    Ok(bytes)
}

/// Copies all of `source` into `target` through `read_at`, so a seed is read
/// from the descriptor its entry validated rather than from a name that may
/// have been repointed, and the descriptor's shared offset is left alone.
fn copy_descriptor(source: &File, target: &mut File) -> Result<(), OperationError> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0_u64;
    loop {
        let read = source.read_at(&mut buffer, offset).map_err(cache_corrupt)?;
        if read == 0 {
            return Ok(());
        }
        target
            .write_all(&buffer[..read])
            .map_err(|error| cache_internal("cannot copy graph seed", error))?;
        offset += read as u64;
    }
}

fn hash_file(file: &File, cancelled: &AtomicBool) -> Result<String, OperationError> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0_u64;
    loop {
        check_cache_cancelled(cancelled)?;
        let read = file.read_at(&mut buffer, offset).map_err(cache_corrupt)?;
        if read == 0 {
            break;
        }
        #[cfg(test)]
        HASH_CHUNK_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        check_cache_cancelled(cancelled)?;
        hasher.update(&buffer[..read]);
        offset += read as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub(crate) fn validate_published_image(path: &Path) -> Result<crate::store::State, OperationError> {
    let state = store::validate_image(path).map_err(cache_corrupt)?;
    let metadata = fs::symlink_metadata(path).map_err(cache_corrupt)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o222 != 0 {
        return Err(cache_corrupt("published graph image is not read-only"));
    }
    Ok(state)
}

pub(crate) fn validate_entry_graph(
    entry: &SnapshotEntry,
    cancelled: &AtomicBool,
) -> Result<crate::store::State, OperationError> {
    if hash_file(&entry.graph_file, cancelled)? != entry.graph_checksum {
        return Err(cache_corrupt("snapshot graph checksum is invalid"));
    }
    validate_pinned_image(&entry.graph_file, &entry.graph_path)
}

fn validate_pinned_graph(
    graph: &PinnedGraph,
    path: &Path,
    cancelled: &AtomicBool,
) -> Result<crate::store::State, OperationError> {
    if hash_file(&graph.file, cancelled)? != graph.checksum {
        return Err(cache_corrupt("snapshot graph checksum is invalid"));
    }
    validate_pinned_image(&graph.file, path)
}

/// Validates the image a descriptor is already held open on. The regular-file
/// and read-only checks run as an `fstat` on that descriptor, and SQLite's own
/// open is pinned to it, so nothing here re-resolves `path` to reach the image.
/// `path` is still the name whose sidecars must be absent.
fn validate_pinned_image(file: &File, path: &Path) -> Result<crate::store::State, OperationError> {
    let metadata = file.metadata().map_err(cache_corrupt)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o222 != 0 {
        return Err(cache_corrupt("published graph image is not read-only"));
    }
    let pin = crate::pinned::pin(path, file).map_err(cache_corrupt)?;
    let state = store::validate_image(path).map_err(cache_corrupt)?;
    pin.require_used().map_err(cache_corrupt)?;
    Ok(state)
}

fn same_file(left: &File, right: &File) -> Result<bool, OperationError> {
    let left = left.metadata().map_err(cache_corrupt)?;
    let right = right.metadata().map_err(cache_corrupt)?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

fn check_cache_cancelled(cancelled: &AtomicBool) -> Result<(), OperationError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(OperationError::new(
            ErrorCode::JobCancelled,
            "snapshot operation was cancelled",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn selected_layers(changes: &WorktreeChanges) -> Vec<ChangeLayer> {
    changes
        .paths
        .iter()
        .flat_map(|path| path.layers.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn changed_file_count(changes: &WorktreeChanges) -> usize {
    changes
        .paths
        .iter()
        .map(|path| path.path.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

fn expected_no_change_reason(
    changes: &WorktreeChanges,
    target: &SnapshotTarget,
    base_oid: &str,
    head_oid: &str,
) -> Option<NoChangeReason> {
    changes.is_empty().then(|| match target {
        SnapshotTarget::Commit if base_oid == head_oid => NoChangeReason::IdenticalCommitOids,
        SnapshotTarget::Commit => NoChangeReason::IdenticalTrees,
        SnapshotTarget::Index => NoChangeReason::EmptyIndexDelta,
        SnapshotTarget::Worktree { .. } => NoChangeReason::EmptyWorktreeDelta,
    })
}

fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_git_oid(id: &str) -> bool {
    matches!(id.len(), 40 | 64)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn snapshot_not_found(snapshot_id: &str) -> OperationError {
    OperationError::new(ErrorCode::SnapshotNotFound, "snapshot is not loaded")
        .with_detail("snapshot_id", snapshot_id)
}

fn cache_corrupt(error: impl std::fmt::Display) -> OperationError {
    OperationError::new(
        ErrorCode::CacheCorrupt,
        format!("cache is corrupt: {error}"),
    )
}

fn cache_internal(context: &str, error: impl std::fmt::Display) -> OperationError {
    OperationError::new(ErrorCode::Internal, format!("{context}: {error}"))
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|error| error.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
pub(crate) struct PublicationPoint {
    pub(crate) snapshot_id: String,
    pub(crate) graph_path: PathBuf,
    pub(crate) review_path: PathBuf,
    pub(crate) manifest_path: PathBuf,
}

#[cfg(test)]
type PublicationHook = Box<dyn FnOnce(&PublicationPoint) -> Result<(), OperationError>>;
#[cfg(test)]
type SeedOpenHook = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
type ReviewHook = Box<dyn FnOnce(&Path) -> Result<(), OperationError>>;
#[cfg(test)]
type DiscoveryHook = Box<dyn FnOnce(&crate::git::Repository)>;

#[cfg(test)]
thread_local! {
    static BEFORE_MANIFEST_HOOK: RefCell<Option<PublicationHook>> = RefCell::new(None);
    static HASH_CHUNK_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    static BEFORE_SEED_OPEN_HOOK: RefCell<Option<SeedOpenHook>> = RefCell::new(None);
    static BEFORE_REVIEW_HOOK: RefCell<Option<ReviewHook>> = RefCell::new(None);
    static BEFORE_GRAPH_LOAD_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    static AFTER_REPOSITORY_DISCOVERY_HOOK: RefCell<Option<DiscoveryHook>> = RefCell::new(None);
}

#[cfg(test)]
fn set_after_repository_discovery_hook_for_test(
    hook: impl FnOnce(&crate::git::Repository) + 'static,
) {
    AFTER_REPOSITORY_DISCOVERY_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn after_repository_discovery(repository: &crate::git::Repository) {
    AFTER_REPOSITORY_DISCOVERY_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(repository);
        }
    });
}

#[cfg(test)]
pub(crate) fn set_before_manifest_hook_for_test(
    hook: impl FnOnce(&PublicationPoint) -> Result<(), OperationError> + 'static,
) {
    BEFORE_MANIFEST_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn before_manifest_publication(point: &PublicationPoint) -> Result<(), OperationError> {
    BEFORE_MANIFEST_HOOK.with(|slot| {
        let hook = slot.borrow_mut().take();
        hook.map_or(Ok(()), |hook| hook(point))
    })
}

#[cfg(test)]
fn set_hash_chunk_hook_for_test(hook: impl FnOnce() + 'static) {
    HASH_CHUNK_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
pub(crate) fn set_before_seed_open_hook_for_test(hook: impl FnOnce(&Path) + 'static) {
    BEFORE_SEED_OPEN_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn set_before_review_hook_for_test(
    hook: impl FnOnce(&Path) -> Result<(), OperationError> + 'static,
) {
    BEFORE_REVIEW_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn before_review_publication(path: &Path) -> Result<(), OperationError> {
    BEFORE_REVIEW_HOOK.with(|slot| {
        let hook = slot.borrow_mut().take();
        hook.map_or(Ok(()), |hook| hook(path))
    })
}

#[cfg(test)]
fn set_before_graph_load_hook_for_test(hook: impl FnOnce() + 'static) {
    BEFORE_GRAPH_LOAD_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn before_graph_load() {
    BEFORE_GRAPH_LOAD_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

pub fn resolve_request(
    roots: &AllowedRoots,
    request: IndexRequest,
    cancelled: &AtomicBool,
) -> Result<ResolvedIndexRequest, OperationError> {
    if let Some(path) = &request.evidence_manifest {
        validate_evidence_path(path)?;
    }
    let root = roots.inspect(&request.worktree_root, cancelled)?;
    let base_oid = resolve_commit(&root.worktree_root, &request.base_ref, "base", cancelled)?;
    let head_oid = resolve_commit(&root.worktree_root, &request.head_ref, "head", cancelled)?;
    if !matches!(request.target, SnapshotTarget::Commit) && head_oid != root.head_oid {
        return Err(OperationError::new(
            ErrorCode::HeadWorktreeMismatch,
            "resolved head does not match worktree HEAD",
        )
        .with_detail("resolved_head_oid", &head_oid)
        .with_detail("worktree_head_oid", &root.head_oid));
    }
    Ok(ResolvedIndexRequest {
        root,
        base_ref: request.base_ref,
        base_oid,
        head_ref: request.head_ref,
        head_oid,
        target: request.target,
        dependency_mode: request.dependency_mode,
        evidence_manifest: request.evidence_manifest,
    })
}

fn validate_evidence_path(path: &Path) -> Result<(), OperationError> {
    let value = path.to_str().ok_or_else(|| {
        OperationError::new(
            ErrorCode::InvalidParameters,
            "evidence manifest path is not valid UTF-8",
        )
    })?;
    if value.is_empty()
        || value.len() > 1024
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || path.is_absolute()
    {
        return Err(OperationError::new(
            ErrorCode::InvalidParameters,
            "evidence manifest must be a safe relative path",
        ));
    }
    Ok(())
}

fn validate_path(path: &Path, label: &str) -> Result<(), OperationError> {
    if !path.is_absolute() {
        return Err(OperationError::new(
            ErrorCode::InvalidParameters,
            format!("{label} must be an absolute path"),
        ));
    }
    let value = path.to_str().ok_or_else(|| {
        OperationError::new(
            ErrorCode::InvalidParameters,
            format!("{label} is not valid UTF-8"),
        )
    })?;
    if value.chars().any(char::is_control) {
        return Err(OperationError::new(
            ErrorCode::InvalidParameters,
            format!("{label} contains control characters"),
        ));
    }
    Ok(())
}

fn identity(repository: Repository) -> RootIdentity {
    let repository_id = hash_fields(
        b"graphr.repository.v2",
        &[
            &repository.common_git_dir,
            &repository.object_format,
            &repository.common_git_dir_dev,
            &repository.common_git_dir_ino,
        ],
    );
    let workspace_id = hash_fields(
        b"graphr.workspace.v1",
        &[
            &repository_id,
            &repository.root,
            &repository.git_dir,
            &repository.index_path,
        ],
    );
    RootIdentity {
        repository_id,
        workspace_id,
        repository_root: repository.root.clone(),
        worktree_root: repository.root,
        git_dir: repository.git_dir,
        common_git_dir: repository.common_git_dir,
        common_git_dir_dev: repository.common_git_dir_dev,
        common_git_dir_ino: repository.common_git_dir_ino,
        index_path: repository.index_path,
        object_format: repository.object_format,
        branch: repository.branch,
        head_oid: repository.head_oid,
    }
}

pub(crate) fn graph_image_key(
    repository_id: &str,
    files: &[CapturedSource],
    omissions: &[SourceOmission],
    cache_format_version: u32,
    analyzer_version: u32,
    schema_version: i64,
) -> String {
    let mut hasher = blake3::Hasher::new();
    b"graphr.graph-image.v1"[..].hash_field(&mut hasher);
    cache_format_version.hash_field(&mut hasher);
    analyzer_version.hash_field(&mut hasher);
    schema_version.hash_field(&mut hasher);
    repository_id.as_bytes().hash_field(&mut hasher);
    (files.len() as u64).hash_field(&mut hasher);
    for file in files {
        file.path.as_bytes().hash_field(&mut hasher);
        file.language.as_str().as_bytes().hash_field(&mut hasher);
        file.content_key.as_bytes().hash_field(&mut hasher);
        file.parse_context.as_bytes().hash_field(&mut hasher);
    }
    (omissions.len() as u64).hash_field(&mut hasher);
    for omission in omissions {
        u32::from(omission.path.is_some()).hash_field(&mut hasher);
        omission
            .path
            .as_deref()
            .unwrap_or_default()
            .as_bytes()
            .hash_field(&mut hasher);
        omission.reason.as_str().as_bytes().hash_field(&mut hasher);
        omission.occurrences.hash_field(&mut hasher);
    }
    hasher.finalize().to_hex().to_string()
}

#[allow(clippy::too_many_arguments)] // The version fields are deliberate cache boundaries.
pub(crate) fn evidence_graph_image_key(
    source_graph_image_id: &str,
    source_snapshot_id: &str,
    manifest_digest: &str,
    artifacts: &[crate::store::ImportedArtifactInput],
    evidence_semantics_version: u32,
    cache_format_version: u32,
    analyzer_version: u32,
    schema_version: i64,
) -> String {
    let mut hasher = blake3::Hasher::new();
    b"graphr.evidence-graph-image.v1"[..].hash_field(&mut hasher);
    source_graph_image_id.as_bytes().hash_field(&mut hasher);
    source_snapshot_id.as_bytes().hash_field(&mut hasher);
    manifest_digest.as_bytes().hash_field(&mut hasher);
    let mut artifacts = artifacts.iter().collect::<Vec<_>>();
    artifacts.sort_unstable_by(|left, right| {
        (left.role, &left.path, left.content_hash, left.byte_size).cmp(&(
            right.role,
            &right.path,
            right.content_hash,
            right.byte_size,
        ))
    });
    (artifacts.len() as u64).hash_field(&mut hasher);
    for artifact in artifacts {
        artifact.role.db().as_bytes().hash_field(&mut hasher);
        artifact.path.as_bytes().hash_field(&mut hasher);
        artifact.content_hash[..].hash_field(&mut hasher);
        artifact.byte_size.hash_field(&mut hasher);
    }
    evidence_semantics_version.hash_field(&mut hasher);
    schema_version.hash_field(&mut hasher);
    analyzer_version.hash_field(&mut hasher);
    cache_format_version.hash_field(&mut hasher);
    hasher.finalize().to_hex().to_string()
}

#[derive(Clone, Copy)]
pub(crate) struct SnapshotKeyInput<'a> {
    pub(crate) graph_image_id: &'a str,
    pub(crate) workspace_id: &'a str,
    pub(crate) base_oid: &'a str,
    pub(crate) head_oid: &'a str,
    pub(crate) target: &'a SnapshotTarget,
    pub(crate) dependency_mode: DependencyMode,
    pub(crate) dirty_digest: &'a str,
    pub(crate) review_id: &'a str,
    pub(crate) source_snapshot_id: Option<&'a str>,
    pub(crate) evidence_manifest_digest: Option<&'a str>,
}

pub(crate) fn snapshot_key(
    input: &SnapshotKeyInput<'_>,
    cache_format_version: u32,
    review_format_version: u32,
) -> String {
    let mut hasher = blake3::Hasher::new();
    b"graphr.snapshot.v2"[..].hash_field(&mut hasher);
    cache_format_version.hash_field(&mut hasher);
    review_format_version.hash_field(&mut hasher);
    input.graph_image_id.as_bytes().hash_field(&mut hasher);
    input.workspace_id.as_bytes().hash_field(&mut hasher);
    input.base_oid.as_bytes().hash_field(&mut hasher);
    input.head_oid.as_bytes().hash_field(&mut hasher);
    match input.target {
        SnapshotTarget::Commit => b"commit"[..].hash_field(&mut hasher),
        SnapshotTarget::Index => b"index"[..].hash_field(&mut hasher),
        SnapshotTarget::Worktree { include_untracked } => {
            b"worktree"[..].hash_field(&mut hasher);
            u32::from(*include_untracked).hash_field(&mut hasher);
        }
    }
    input
        .dependency_mode
        .as_str()
        .as_bytes()
        .hash_field(&mut hasher);
    input.dirty_digest.as_bytes().hash_field(&mut hasher);
    input.review_id.as_bytes().hash_field(&mut hasher);
    input
        .source_snapshot_id
        .unwrap_or_default()
        .as_bytes()
        .hash_field(&mut hasher);
    input
        .evidence_manifest_digest
        .unwrap_or_default()
        .as_bytes()
        .hash_field(&mut hasher);
    hasher.finalize().to_hex().to_string()
}

trait HashField {
    fn hash_field(&self, hasher: &mut blake3::Hasher);
}

impl HashField for PathBuf {
    fn hash_field(&self, hasher: &mut blake3::Hasher) {
        self.to_string_lossy().as_bytes().hash_field(hasher);
    }
}

impl HashField for String {
    fn hash_field(&self, hasher: &mut blake3::Hasher) {
        self.as_bytes().hash_field(hasher);
    }
}

impl HashField for [u8] {
    fn hash_field(&self, hasher: &mut blake3::Hasher) {
        hasher.update(&(self.len() as u64).to_le_bytes());
        hasher.update(self);
    }
}

impl HashField for u32 {
    fn hash_field(&self, hasher: &mut blake3::Hasher) {
        self.to_le_bytes()[..].hash_field(hasher);
    }
}

impl HashField for u64 {
    fn hash_field(&self, hasher: &mut blake3::Hasher) {
        self.to_le_bytes()[..].hash_field(hasher);
    }
}

impl HashField for i64 {
    fn hash_field(&self, hasher: &mut blake3::Hasher) {
        self.to_le_bytes()[..].hash_field(hasher);
    }
}

fn hash_fields(domain: &[u8], fields: &[&dyn HashField]) -> String {
    let mut hasher = blake3::Hasher::new();
    domain.hash_field(&mut hasher);
    for field in fields {
        field.hash_field(&mut hasher);
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use crate::git::{
        CapturedSource, DependencyMode, Language, SourceContent, SourceOmission,
        SourceOmissionReason,
    };
    use crate::index::Engine;
    use crate::store;

    use super::{
        AllowedRoots, CacheDirectory, ErrorCode, IndexRequest, OperationError, PublicationPoint,
        SnapshotCatalog, SnapshotKeyInput, SnapshotTarget, evidence_graph_image_key,
        graph_image_key, read_dir_at, remove_tree_at, resolve_request,
        set_after_repository_discovery_hook_for_test, set_before_manifest_hook_for_test,
        set_before_review_hook_for_test, snapshot_key,
    };

    #[test]
    fn enumeration_reads_every_name_and_leaves_the_directory_handle_usable() {
        let root = temp_root("enumerate");
        fs::create_dir_all(&root).unwrap();
        for name in ["first.json", "second.json"] {
            fs::write(root.join(name), b"{}").unwrap();
        }
        // APFS refuses a non-UTF-8 name with EILSEQ where ext4 accepts it, so
        // byte preservation is asserted wherever the filesystem can express it.
        let raw = OsStr::from_bytes(b"invalid-\xff-name.json");
        let raw_exists = fs::write(root.join(raw), b"{}").is_ok();
        fs::create_dir(root.join("nested")).unwrap();
        let directory =
            CacheDirectory::open_root(&root, &fs::symlink_metadata(&root).unwrap()).unwrap();

        let mut first = read_dir_at(&directory).unwrap();
        let mut second = read_dir_at(&directory).unwrap();
        first.sort();
        second.sort();

        let mut expected = vec![
            OsString::from("first.json"),
            OsString::from("nested"),
            OsString::from("second.json"),
        ];
        if raw_exists {
            expected.push(raw.to_owned());
        }
        expected.sort();
        assert_eq!(first, expected);
        assert_eq!(first, second, "a shared directory position would drift");
        directory
            .sync()
            .expect("the pinned handle survives closedir");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tree_removal_descends_through_descriptors() {
        let root = temp_root("remove-tree");
        fs::create_dir_all(root.join("job/capture/src")).unwrap();
        fs::write(root.join("job/capture/src/a.rs"), b"fn a() {}\n").unwrap();
        fs::write(root.join("job/graph.db"), b"image").unwrap();
        let directory =
            CacheDirectory::open_root(&root, &fs::symlink_metadata(&root).unwrap()).unwrap();

        remove_tree_at(&directory, OsStr::new("job")).unwrap();

        assert!(!root.join("job").exists());
        assert!(read_dir_at(&directory).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graph_image_key_covers_repository_content_context_and_versions() {
        fn source(
            path: &str,
            content_key: &str,
            language: Language,
            context: &str,
        ) -> CapturedSource {
            CapturedSource {
                path: path.into(),
                language,
                git_oid: Some(content_key.into()),
                content_key: content_key.into(),
                parse_context: context.into(),
                content: SourceContent::GitBlob(content_key.into()),
            }
        }

        let files = vec![source("src/lib.rs", "a", Language::Rust, "crate")];
        let key = graph_image_key("repository", &files, &[], 6, 1, 4);
        assert_ne!(key, graph_image_key("other", &files, &[], 6, 1, 4));
        assert_ne!(
            key,
            graph_image_key(
                "repository",
                &[source("src/lib.rs", "b", Language::Rust, "crate")],
                &[],
                6,
                1,
                4,
            )
        );
        assert_ne!(
            key,
            graph_image_key(
                "repository",
                &[source("src/lib.rs", "a", Language::Python, "crate")],
                &[],
                6,
                1,
                4,
            )
        );
        assert_ne!(
            key,
            graph_image_key(
                "repository",
                &[source("src/lib.rs", "a", Language::Rust, "other")],
                &[],
                6,
                1,
                4,
            )
        );
        assert_ne!(key, graph_image_key("repository", &files, &[], 7, 1, 4));
        assert_ne!(key, graph_image_key("repository", &files, &[], 6, 2, 4));
        assert_ne!(key, graph_image_key("repository", &files, &[], 6, 1, 5));
    }

    #[test]
    fn graph_image_key_changes_with_source_omissions() {
        let omissions = [SourceOmission {
            path: Some("src/large.rs".into()),
            reason: SourceOmissionReason::Oversized,
            occurrences: 1,
        }];
        assert_ne!(
            graph_image_key("repository", &[], &[], 7, 3, 5),
            graph_image_key("repository", &[], &omissions, 7, 3, 5)
        );
    }

    #[test]
    fn evidence_graph_image_key_covers_source_and_artifact_identity() {
        use crate::store::{ArtifactRole, ImportedArtifactInput};

        let artifact = ImportedArtifactInput {
            key: "output".into(),
            path: "target/out.rs".into(),
            role: ArtifactRole::GeneratedRust,
            content_hash: [7; 32],
            byte_size: 42,
        };
        let key = evidence_graph_image_key(
            "source-graph",
            &"a".repeat(64),
            &"b".repeat(64),
            std::slice::from_ref(&artifact),
            1,
            8,
            4,
            6,
        );
        assert_ne!(
            key,
            evidence_graph_image_key(
                "other-source-graph",
                &"a".repeat(64),
                &"b".repeat(64),
                &[artifact],
                1,
                8,
                4,
                6,
            )
        );
    }

    #[test]
    fn snapshot_key_covers_workspace_range_target_digest_and_review_version() {
        let input = SnapshotKeyInput {
            graph_image_id: "graph",
            workspace_id: "workspace",
            base_oid: "base",
            head_oid: "head",
            target: &SnapshotTarget::Worktree {
                include_untracked: true,
            },
            dependency_mode: DependencyMode::Boundary,
            dirty_digest: "dirty",
            review_id: "review",
            source_snapshot_id: None,
            evidence_manifest_digest: None,
        };
        let key = snapshot_key(&input, 6, 2);
        for changed in [
            SnapshotKeyInput {
                workspace_id: "other",
                ..input
            },
            SnapshotKeyInput {
                base_oid: "other",
                ..input
            },
            SnapshotKeyInput {
                head_oid: "other",
                ..input
            },
            SnapshotKeyInput {
                target: &SnapshotTarget::Index,
                ..input
            },
            SnapshotKeyInput {
                dependency_mode: DependencyMode::Full,
                ..input
            },
            SnapshotKeyInput {
                dirty_digest: "other",
                ..input
            },
            SnapshotKeyInput {
                review_id: "other",
                ..input
            },
        ] {
            assert_ne!(key, snapshot_key(&changed, 6, 2));
        }
        assert_ne!(key, snapshot_key(&input, 7, 2));
        assert_ne!(key, snapshot_key(&input, 6, 3));
    }

    #[test]
    fn publication_is_atomic_and_manifest_is_last() {
        let root = repository_with_source("atomic-publication", "fn first() {}\n");
        fs::create_dir(root.join(".git/graphr")).unwrap();
        let legacy = root.join(".git/graphr/index.db");
        fs::write(&legacy, b"legacy database must stay untouched").unwrap();
        let engine = test_engine(&root);
        let request = resolved_commit(&engine, &root, "HEAD", "HEAD");
        let racing_request = request.clone();
        let observed = Arc::new(AtomicBool::new(false));
        let racing_completion = Arc::new(Mutex::new(None));
        set_before_manifest_hook_for_test({
            let engine = engine.clone();
            let observed = observed.clone();
            let racing_completion = racing_completion.clone();
            move |point: &PublicationPoint| {
                assert!(!point.manifest_path.exists());
                store::validate_image(&point.graph_path).unwrap();
                let review = fs::read(&point.review_path).unwrap();
                rmcp::serde_json::from_slice::<crate::git::WorktreeChanges>(&review).unwrap();
                assert_eq!(
                    engine.snapshot(&point.snapshot_id).unwrap_err().code,
                    ErrorCode::SnapshotNotFound
                );
                observed.store(true, std::sync::atomic::Ordering::Relaxed);
                *racing_completion.lock().unwrap() =
                    Some(engine.build_snapshot(racing_request, &AtomicBool::new(false), |_| {})?);
                Ok(())
            }
        });

        let completion = engine
            .build_snapshot(request, &AtomicBool::new(false), |_| {})
            .unwrap();

        assert!(observed.load(std::sync::atomic::Ordering::Relaxed));
        let racing = racing_completion.lock().unwrap().take().unwrap();
        assert_eq!(racing.snapshot_id, completion.snapshot_id);
        assert_eq!(racing.stats.files_reused, racing.stats.files_total);
        let entry = engine.snapshot(&completion.snapshot_id).unwrap();
        assert_eq!(entry.graph_image_id, completion.graph_image_id);
        store::validate_image(&entry.graph_path).unwrap();
        assert_eq!(
            fs::read(&legacy).unwrap(),
            b"legacy database must stay untouched"
        );

        let roots = Arc::new(AllowedRoots::new(vec![root.clone()]).unwrap());
        let identity = roots.inspect(&root, &AtomicBool::new(false)).unwrap();
        let fresh = SnapshotCatalog::new(roots);
        assert_eq!(
            fresh.get(&completion.snapshot_id).unwrap_err().code,
            ErrorCode::SnapshotNotFound
        );
        fresh.attach(&identity, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            fresh.get(&completion.snapshot_id).unwrap().graph_image_id,
            completion.graph_image_id
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_wins_cancellation_after_manifest_marker() {
        let root = repository_with_source("published-before-cancel", "fn source() {}\n");
        let engine = test_engine(&root);
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        super::set_before_graph_load_hook_for_test({
            let cancelled = cancelled.clone();
            let observed = observed.clone();
            let snapshots = root.join(".git/graphr/v6/snapshots");
            move || {
                assert_eq!(fs::read_dir(snapshots).unwrap().count(), 1);
                observed.store(true, std::sync::atomic::Ordering::Release);
                cancelled.store(true, std::sync::atomic::Ordering::Release);
            }
        });

        let completion = engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD", "HEAD"),
                &cancelled,
                |_| {},
            )
            .unwrap();

        assert!(observed.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            engine.snapshot(&completion.snapshot_id).unwrap().provenance,
            completion.provenance
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_reuse_does_not_publish_a_replaced_graph_name() {
        let root = repository_with_source("exact-reuse-replacement", "fn original() {}\n");
        let engine = test_engine(&root);
        let original_request = resolved_commit(&engine, &root, "HEAD", "HEAD");
        let original = engine
            .build_snapshot(original_request.clone(), &AtomicBool::new(false), |_| {})
            .unwrap();
        let node_ref = engine
            .search(&original.snapshot_id, "original", Some("function"), 8)
            .unwrap()
            .text
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        fs::write(root.join("src/lib.rs"), "fn replacement() {}\n").unwrap();
        test_git(&root, &["commit", "--quiet", "-am", "replacement"]);
        let replacement = engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let graph_directory = root.join(".git/graphr/v6/graphs");
        let original_graph = graph_directory.join(format!("{}.db", original.graph_image_id));
        let replacement_graph = graph_directory.join(format!("{}.db", replacement.graph_image_id));
        let manifest = root
            .join(".git/graphr/v6/snapshots")
            .join(format!("{}.json", original.snapshot_id));
        set_before_review_hook_for_test({
            let manifest = manifest.clone();
            move |_| {
                fs::remove_file(&manifest).unwrap();
                fs::rename(&original_graph, original_graph.with_extension("validated")).unwrap();
                fs::copy(&replacement_graph, &original_graph).unwrap();
                Ok(())
            }
        });

        let error = engine
            .build_snapshot(original_request, &AtomicBool::new(false), |_| {})
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::CacheCorrupt);
        assert!(!manifest.exists());
        let search = engine
            .search(&original.snapshot_id, "original", Some("function"), 8)
            .unwrap();
        assert!(search.text.contains("original"), "{}", search.text);
        assert!(!search.text.contains("replacement"), "{}", search.text);
        let view = engine
            .view(&original.snapshot_id, &node_ref, 1, 30)
            .unwrap();
        assert!(view.text.contains("original"), "{}", view.text);
        assert!(!view.text.contains("replacement"), "{}", view.text);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_cache_is_quarantined_before_rebuild() {
        let root = repository_with_source("corrupt-cache", "fn first() {}\n");
        let engine = test_engine(&root);
        let request = resolved_commit(&engine, &root, "HEAD", "HEAD");
        let first = engine
            .build_snapshot(request.clone(), &AtomicBool::new(false), |_| {})
            .unwrap();
        let graph = engine
            .snapshot(&first.snapshot_id)
            .unwrap()
            .graph_path
            .clone();
        fs::set_permissions(&graph, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        let rejected = Mutex::new(Vec::new());

        let rebuilt = engine
            .build_snapshot(request.clone(), &AtomicBool::new(false), |progress| {
                if let Some(path) = progress.rejected_cache {
                    rejected.lock().unwrap().push(path);
                }
            })
            .unwrap();

        assert_eq!(rebuilt.snapshot_id, first.snapshot_id);
        assert!(!rejected.into_inner().unwrap().is_empty());
        assert!(
            fs::read_dir(root.join(".git/graphr/v6/quarantine"))
                .unwrap()
                .next()
                .is_some()
        );
        store::validate_image(&engine.snapshot(&rebuilt.snapshot_id).unwrap().graph_path).unwrap();

        let manifest_path = root
            .join(".git/graphr/v6/snapshots")
            .join(format!("{}.json", rebuilt.snapshot_id));
        let manifest: super::SnapshotManifest =
            rmcp::serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let review_path = root
            .join(".git/graphr/v6/reviews")
            .join(format!("{}.json", manifest.review_id));
        fs::write(&review_path, b"corrupt review").unwrap();
        let rejected = Mutex::new(Vec::new());

        let exact = engine
            .build_snapshot(request, &AtomicBool::new(false), |progress| {
                if let Some(path) = progress.rejected_cache {
                    rejected.lock().unwrap().push(path);
                }
            })
            .unwrap();

        assert_eq!(exact.stats.files_reused, exact.stats.files_total);
        assert_eq!(exact.stats.files_parsed, 0);
        assert!(!rejected.into_inner().unwrap().is_empty());
        assert_eq!(
            engine
                .snapshot(&exact.snapshot_id)
                .unwrap()
                .changes
                .paths
                .len(),
            0
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejected_cache_is_quarantined_only_by_its_repository() {
        let first_root = repository_with_source("rejected-first", "fn first() {}\n");
        let second_root = repository_with_source("rejected-second", "fn second() {}\n");
        let first_engine = test_engine(&first_root);
        let completion = first_engine
            .build_snapshot(
                resolved_commit(&first_engine, &first_root, "HEAD", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let manifest_path = first_root
            .join(".git/graphr/v6/snapshots")
            .join(format!("{}.json", completion.snapshot_id));
        let manifest: super::SnapshotManifest =
            rmcp::serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        fs::write(
            first_root
                .join(".git/graphr/v6/reviews")
                .join(format!("{}.json", manifest.review_id)),
            b"corrupt",
        )
        .unwrap();

        let roots =
            Arc::new(AllowedRoots::new(vec![first_root.clone(), second_root.clone()]).unwrap());
        let first = roots.inspect(&first_root, &AtomicBool::new(false)).unwrap();
        let second = roots
            .inspect(&second_root, &AtomicBool::new(false))
            .unwrap();
        let catalog = SnapshotCatalog::new(roots);
        catalog.attach(&first, &AtomicBool::new(false)).unwrap();

        catalog
            .quarantine_rejected(&second, &"b".repeat(64))
            .unwrap();

        assert!(manifest_path.exists());
        assert_eq!(
            catalog.get(&completion.snapshot_id).unwrap_err().code,
            ErrorCode::CacheCorrupt
        );
        fs::remove_dir_all(first_root).unwrap();
        fs::remove_dir_all(second_root).unwrap();
    }

    #[test]
    fn attach_evicts_loaded_snapshot_after_manifest_disappears() {
        let root = repository_with_source("manifest-removed", "fn source() {}\n");
        let engine = test_engine(&root);
        let completion = engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let roots = Arc::new(AllowedRoots::new(vec![root.clone()]).unwrap());
        let identity = roots.inspect(&root, &AtomicBool::new(false)).unwrap();
        let catalog = SnapshotCatalog::new(roots);
        catalog.attach(&identity, &AtomicBool::new(false)).unwrap();
        catalog.get(&completion.snapshot_id).unwrap();
        fs::remove_file(
            root.join(".git/graphr/v6/snapshots")
                .join(format!("{}.json", completion.snapshot_id)),
        )
        .unwrap();

        catalog.attach(&identity, &AtomicBool::new(false)).unwrap();

        assert_eq!(
            catalog.get(&completion.snapshot_id).unwrap_err().code,
            ErrorCode::SnapshotNotFound
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_rejects_oversized_review_before_sidecar() {
        let root = repository_with_source("oversized-review", "fn source() {}\n");
        let engine = test_engine(&root);
        let completion = engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let oversized = vec![b'x'; super::REVIEW_SIZE_LIMIT as usize + 1];
        let review_id = blake3::hash(&oversized).to_hex().to_string();
        let roots = Arc::new(AllowedRoots::new(vec![root.clone()]).unwrap());
        let identity = roots.inspect(&root, &AtomicBool::new(false)).unwrap();
        let catalog = SnapshotCatalog::new(roots);
        let job = catalog.begin(&identity).unwrap();
        let mut provenance = completion.provenance;
        provenance.snapshot_id = "a".repeat(64);

        let error = catalog
            .publish(
                &job,
                &completion.graph_image_id,
                &review_id,
                &oversized,
                None,
                None,
                DependencyMode::Boundary,
                None,
                provenance,
                &AtomicBool::new(false),
            )
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Internal);
        assert!(
            !root
                .join(".git/graphr/v6/reviews")
                .join(format!("{review_id}.json"))
                .exists()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_rejects_oversized_manifest_before_marker() {
        let root = repository_with_source("oversized-manifest", "fn source() {}\n");
        let engine = test_engine(&root);
        let mut request = resolved_commit(&engine, &root, "HEAD", "HEAD");
        request.base_ref = "x".repeat(super::MANIFEST_SIZE_LIMIT as usize);

        let error = engine
            .build_snapshot(request, &AtomicBool::new(false), |_| {})
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(
            fs::read_dir(root.join(".git/graphr/v6/snapshots"))
                .unwrap()
                .count(),
            0
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bounded_reader_stops_after_limit_plus_one_when_file_grows() {
        let mut reader = std::io::Cursor::new(vec![b'x'; 32]);

        let error = super::read_exact_bounded(&mut reader, 1, 4).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(reader.position(), 5);
    }

    #[test]
    fn attach_honors_cancellation_during_graph_hashing() {
        let root = repository_with_source("attach-cancel", "fn source() {}\n");
        let engine = test_engine(&root);
        engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let roots = Arc::new(AllowedRoots::new(vec![root.clone()]).unwrap());
        let identity = roots.inspect(&root, &AtomicBool::new(false)).unwrap();
        let catalog = SnapshotCatalog::new(roots);
        let cancelled = Arc::new(AtomicBool::new(false));
        super::set_hash_chunk_hook_for_test({
            let cancelled = cancelled.clone();
            move || cancelled.store(true, std::sync::atomic::Ordering::Relaxed)
        });

        let error = catalog.attach(&identity, &cancelled).unwrap_err();

        assert_eq!(error.code, ErrorCode::JobCancelled);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_publication_preserves_the_previous_snapshot() {
        let root = repository_with_source("publication-rollback", "fn first() {}\n");
        fs::write(root.join("src/lib.rs"), "fn second() {}\n").unwrap();
        test_git(&root, &["commit", "--quiet", "-am", "second"]);
        let engine = test_engine(&root);
        let first = engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD~1", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        fs::write(root.join("src/lib.rs"), "fn third() {}\n").unwrap();
        test_git(&root, &["commit", "--quiet", "-am", "third"]);
        set_before_manifest_hook_for_test(|_| {
            Err(OperationError::new(
                ErrorCode::Internal,
                "injected publication failure",
            ))
        });

        let error = engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD~1", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Internal);
        store::validate_image(&engine.snapshot(&first.snapshot_id).unwrap().graph_path).unwrap();
        assert_eq!(
            fs::read_dir(root.join(".git/graphr/v6/snapshots"))
                .unwrap()
                .count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_namespace_rejects_symlink_components() {
        let root = repository_with_source("cache-symlink", "fn source() {}\n");
        let outside = temp_root("cache-symlink-outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".git/graphr")).unwrap();
        let engine = test_engine(&root);

        let error = engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::CacheCorrupt);
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        fs::remove_file(root.join(".git/graphr")).unwrap();
        fs::remove_dir_all(outside).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    /// Pins the coupling that makes unconditional `SQLITE_OPEN_NOFOLLOW` safe.
    ///
    /// That flag rejects a symlinked *ancestor*, not merely a symlinked final
    /// component, so every database open would fail deep inside SQLite if the
    /// cache were ever rooted at a path that is not already its own
    /// canonicalisation. `cache_paths` is what guarantees it is, and this test
    /// fails if that clause is relaxed.
    #[test]
    fn cache_paths_rejects_a_non_canonical_common_git_dir() {
        let root = repository_with_source("cache-noncanonical", "fn source() {}\n");
        let roots = AllowedRoots::new(vec![root.clone()]).unwrap();
        let identity = roots.inspect(&root, &AtomicBool::new(false)).unwrap();
        // Control: the canonical identity is accepted, so the only difference
        // below is the symlinked ancestor.
        assert!(super::cache_paths(&identity, false).is_ok());

        let link = temp_root("cache-noncanonical-link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let mut aliased = identity.clone();
        // Both fields move together, so `git_dir.starts_with(common_git_dir)`
        // still holds and only the canonicalisation clause can reject this.
        aliased.common_git_dir = link.join(".git");
        aliased.git_dir = link.join(".git");

        let Err(error) = super::cache_paths(&aliased, false) else {
            panic!("a common Git directory reached through a symlink must be rejected");
        };

        assert_eq!(error.code, ErrorCode::GitMetadataInvalid);
        fs::remove_file(&link).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_uses_validated_directory_after_component_swap() {
        let root = repository_with_source("cache-component-swap", "fn source() {}\n");
        let outside = temp_root("cache-component-swap-outside");
        fs::create_dir(&outside).unwrap();
        let engine = test_engine(&root);
        let original = root.join(".git/graphr/v6/reviews-original");
        super::set_before_review_hook_for_test({
            let outside = outside.clone();
            let original = original.clone();
            move |review_path| {
                let reviews = review_path.parent().unwrap();
                fs::rename(reviews, &original).unwrap();
                std::os::unix::fs::symlink(&outside, reviews).unwrap();
                Ok(())
            }
        });

        let completion = engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();

        store::Store::open_reader(&engine.snapshot(&completion.snapshot_id).unwrap().graph_path)
            .unwrap();
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&original).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn attachment_does_not_follow_graph_directory_created_after_validation() {
        let root = repository_with_source("missing-graph-swap", "fn source() {}\n");
        let engine = test_engine(&root);
        let completion = engine
            .build_snapshot(
                resolved_commit(&engine, &root, "HEAD", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let graph_directory = root.join(".git/graphr/v6/graphs");
        let outside = root.join("outside-graphs");
        fs::rename(&graph_directory, &outside).unwrap();
        super::set_before_graph_load_hook_for_test({
            let graph_directory = graph_directory.clone();
            let outside = outside.clone();
            move || std::os::unix::fs::symlink(outside, graph_directory).unwrap()
        });
        let roots = Arc::new(AllowedRoots::new(vec![root.clone()]).unwrap());
        let identity = roots.inspect(&root, &AtomicBool::new(false)).unwrap();
        let catalog = SnapshotCatalog::new(roots);

        catalog.attach(&identity, &AtomicBool::new(false)).unwrap();

        assert_eq!(
            catalog.get(&completion.snapshot_id).unwrap_err().code,
            ErrorCode::SnapshotIncomplete
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attached_manifest_reauthorizes_its_worktree() {
        let fixture = linked_worktrees("manifest-authorization");
        let engine = Engine::new(Arc::new(
            AllowedRoots::new(vec![fixture.main.clone(), fixture.linked.clone()]).unwrap(),
        ));
        let completion = engine
            .build_snapshot(
                resolved_commit(&engine, &fixture.linked, "HEAD", "HEAD"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let roots = Arc::new(AllowedRoots::new(vec![fixture.main.clone()]).unwrap());
        let identity = roots
            .inspect(&fixture.main, &AtomicBool::new(false))
            .unwrap();
        let catalog = SnapshotCatalog::new(roots);

        catalog.attach(&identity, &AtomicBool::new(false)).unwrap();

        assert_eq!(
            catalog.get(&completion.snapshot_id).unwrap_err().code,
            ErrorCode::RootDisallowed
        );
    }

    #[test]
    fn resolve_request_pins_base_and_head_oids() {
        let root = temp_root("resolve-request");
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet", "--initial-branch=main"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(root.join("base.rs"), "fn base() {}\n").unwrap();
        test_git(&root, &["add", "--", "base.rs"]);
        test_git(&root, &["commit", "--quiet", "-m", "base"]);
        test_git(&root, &["switch", "--quiet", "-c", "feature"]);
        fs::write(root.join("head.rs"), "fn head() {}\n").unwrap();
        test_git(&root, &["add", "--", "head.rs"]);
        test_git(&root, &["commit", "--quiet", "-m", "head"]);
        test_git(&root, &["switch", "--quiet", "main"]);

        let expected_base = git_required_line(&root, &["rev-parse", "main^{commit}"]);
        let expected_head = git_required_line(&root, &["rev-parse", "feature^{commit}"]);
        let roots = AllowedRoots::new(vec![root.clone()]).unwrap();
        let resolved = resolve_request(
            &roots,
            IndexRequest {
                worktree_root: root.clone(),
                base_ref: "main".into(),
                head_ref: "feature".into(),
                target: SnapshotTarget::Commit,
                dependency_mode: DependencyMode::Boundary,
                evidence_manifest: None,
            },
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(resolved.base_oid, expected_base);
        assert_eq!(resolved.head_oid, expected_head);
        assert_eq!(resolved.base_ref, "main");
        assert_eq!(resolved.head_ref, "feature");
        assert_eq!(
            resolved.root.worktree_root,
            fs::canonicalize(&root).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_request_rejects_a_head_that_differs_from_the_worktree() {
        let root = temp_root("head-mismatch");
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet", "--initial-branch=main"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(root.join("source.rs"), "fn first() {}\n").unwrap();
        test_git(&root, &["add", "--", "source.rs"]);
        test_git(&root, &["commit", "--quiet", "-m", "first"]);
        fs::write(root.join("source.rs"), "fn second() {}\n").unwrap();
        test_git(&root, &["commit", "--quiet", "-am", "second"]);
        let worktree_head = git_required_line(&root, &["rev-parse", "HEAD"]);
        let resolved_head = git_required_line(&root, &["rev-parse", "HEAD~1"]);
        let roots = AllowedRoots::new(vec![root.clone()]).unwrap();

        let error = resolve_request(
            &roots,
            IndexRequest {
                worktree_root: root.clone(),
                base_ref: "HEAD~1".into(),
                head_ref: "HEAD~1".into(),
                target: SnapshotTarget::Index,
                dependency_mode: DependencyMode::Boundary,
                evidence_manifest: None,
            },
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::HeadWorktreeMismatch);
        assert_eq!(error.details["resolved_head_oid"], resolved_head);
        assert_eq!(error.details["worktree_head_oid"], worktree_head);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_request_rejects_invalid_revision_text_before_git() {
        let root = temp_root("invalid-revisions");
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet", "--initial-branch=main"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(root.join("source.rs"), "fn source() {}\n").unwrap();
        test_git(&root, &["add", "--", "source.rs"]);
        test_git(&root, &["commit", "--quiet", "-m", "source"]);
        let roots = AllowedRoots::new(vec![root.clone()]).unwrap();

        for revision in [
            String::new(),
            "-option".into(),
            "x".repeat(257),
            "bad\nref".into(),
        ] {
            let error = resolve_request(
                &roots,
                IndexRequest {
                    worktree_root: root.clone(),
                    base_ref: revision,
                    head_ref: "HEAD".into(),
                    target: SnapshotTarget::Commit,
                    dependency_mode: DependencyMode::Boundary,
                    evidence_manifest: None,
                },
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidParameters);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inspect_reports_common_and_per_worktree_identity() {
        let fixture = linked_worktrees("identity");
        let allowed =
            AllowedRoots::new(vec![fixture.main.clone(), fixture.linked.clone()]).unwrap();
        let cancelled = AtomicBool::new(false);

        let main = allowed.inspect(&fixture.main, &cancelled).unwrap();
        let linked = allowed.inspect(&fixture.linked, &cancelled).unwrap();

        assert_eq!(main.common_git_dir, linked.common_git_dir);
        assert_eq!(main.repository_id, linked.repository_id);
        assert_eq!(main.object_format, linked.object_format);
        assert_ne!(main.worktree_root, linked.worktree_root);
        assert_ne!(main.git_dir, linked.git_dir);
        assert_ne!(main.index_path, linked.index_path);
        assert_ne!(main.workspace_id, linked.workspace_id);
        assert_eq!(main.repository_root, main.worktree_root);
        assert_eq!(linked.repository_root, linked.worktree_root);
        assert_eq!(
            main.branch,
            git_line(
                &fixture.main,
                &["symbolic-ref", "--quiet", "--short", "HEAD"]
            )
        );
        assert_eq!(
            linked.branch,
            git_line(
                &fixture.linked,
                &["symbolic-ref", "--quiet", "--short", "HEAD"]
            )
        );
        assert_eq!(
            main.head_oid,
            git_required_line(&fixture.main, &["rev-parse", "--verify", "HEAD^{commit}"])
        );
        assert_eq!(
            linked.head_oid,
            git_required_line(&fixture.linked, &["rev-parse", "--verify", "HEAD^{commit}"])
        );
        assert_ne!(main.head_oid, linked.head_oid);
    }

    #[test]
    fn identity_uses_the_common_git_directory_validated_during_discovery() {
        let root = repository_with_source("identity-boundary", "fn source() {}\n");
        let roots = AllowedRoots::new(vec![root.clone()]).unwrap();
        let cancelled = AtomicBool::new(false);
        let original = roots.inspect(&root, &cancelled).unwrap();
        let git_dir = root.join(".git");
        let moved = root.join(".git-original");

        set_after_repository_discovery_hook_for_test(move |_| {
            fs::rename(&git_dir, &moved).unwrap();
            fs::create_dir(&git_dir).unwrap();
        });
        let raced = roots.inspect(&root, &cancelled).unwrap();

        let raced_repository_id = raced.repository_id;
        let raced_workspace_id = raced.workspace_id;
        fs::remove_dir_all(root.join(".git")).unwrap();
        fs::rename(root.join(".git-original"), root.join(".git")).unwrap();
        fs::remove_dir_all(root).unwrap();
        assert_eq!(raced_repository_id, original.repository_id);
        assert_eq!(raced_workspace_id, original.workspace_id);
    }

    #[test]
    fn inspect_rejects_disallowed_stale_subdirectory_and_symlink_escape() {
        let fixture = linked_worktrees("rejections");
        let cancelled = AtomicBool::new(false);

        let disallowed = AllowedRoots::new(vec![fixture.main.clone()]).unwrap();
        assert_eq!(
            disallowed
                .inspect(&fixture.linked, &cancelled)
                .unwrap_err()
                .code,
            ErrorCode::RootDisallowed
        );

        let stale_path = temp_root("stale");
        fs::create_dir_all(&stale_path).unwrap();
        let stale = AllowedRoots::new(vec![stale_path.clone()]).unwrap();
        let replacement = temp_root("stale-replacement");
        fs::rename(&stale_path, &replacement).unwrap();
        fs::create_dir(&stale_path).unwrap();
        assert_eq!(
            stale.inspect(&stale_path, &cancelled).unwrap_err().code,
            ErrorCode::RootStale
        );

        let subdirectory = fixture.main.join("src");
        fs::create_dir(&subdirectory).unwrap();
        let allowed = AllowedRoots::new(vec![fixture.main.clone()]).unwrap();
        assert_eq!(
            allowed.inspect(&subdirectory, &cancelled).unwrap_err().code,
            ErrorCode::RootNotWorktree
        );

        let git_dir = PathBuf::from(git_required_line(
            &fixture.linked,
            &["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
        ));
        let escaped_git_dir = temp_root("escaped-git-dir");
        fs::rename(&git_dir, &escaped_git_dir).unwrap();
        std::os::unix::fs::symlink(&escaped_git_dir, &git_dir).unwrap();
        let invalid_allowed = AllowedRoots::new(vec![fixture.linked.clone()]).unwrap();
        assert_eq!(
            invalid_allowed
                .inspect(&fixture.linked, &cancelled)
                .unwrap_err()
                .code,
            ErrorCode::GitMetadataInvalid
        );
        fs::remove_file(&git_dir).unwrap();
        fs::rename(&escaped_git_dir, &git_dir).unwrap();

        fs::remove_dir_all(replacement).unwrap();
        fs::remove_dir_all(stale_path).unwrap();
    }

    struct LinkedWorktrees {
        root: PathBuf,
        main: PathBuf,
        linked: PathBuf,
    }

    impl Drop for LinkedWorktrees {
        fn drop(&mut self) {
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&self.linked)
                .current_dir(&self.main)
                .status();
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn linked_worktrees(label: &str) -> LinkedWorktrees {
        let root = temp_root(label);
        let main = root.join("main");
        let linked = root.join("linked");
        fs::create_dir_all(&main).unwrap();
        test_git(&main, &["init", "--quiet"]);
        test_git(&main, &["config", "user.name", "Graphr Test"]);
        test_git(&main, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(main.join("baseline.txt"), "baseline\n").unwrap();
        test_git(&main, &["add", "--", "baseline.txt"]);
        test_git(&main, &["commit", "--quiet", "-m", "baseline"]);
        test_git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );
        fs::write(linked.join("linked.txt"), "linked\n").unwrap();
        test_git(&linked, &["add", "--", "linked.txt"]);
        test_git(&linked, &["commit", "--quiet", "-m", "linked"]);
        LinkedWorktrees { root, main, linked }
    }

    fn repository_with_source(label: &str, source: &str) -> PathBuf {
        let root = temp_root(label);
        fs::create_dir_all(root.join("src")).unwrap();
        test_git(&root, &["init", "--quiet", "--initial-branch=main"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(root.join("src/lib.rs"), source).unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "first"]);
        root
    }

    fn test_engine(root: &Path) -> Arc<Engine> {
        Arc::new(Engine::new(Arc::new(
            AllowedRoots::new(vec![root.to_path_buf()]).unwrap(),
        )))
    }

    fn resolved_commit(
        engine: &Engine,
        root: &Path,
        base: &str,
        head: &str,
    ) -> super::ResolvedIndexRequest {
        resolve_request(
            engine.roots(),
            IndexRequest {
                worktree_root: root.to_path_buf(),
                base_ref: base.into(),
                head_ref: head.into(),
                target: SnapshotTarget::Commit,
                dependency_mode: DependencyMode::Boundary,
                evidence_manifest: None,
            },
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    fn test_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
    }

    fn git_line(root: &Path, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    fn git_required_line(root: &Path, args: &[&str]) -> String {
        git_line(root, args).unwrap()
    }

    fn temp_root(label: &str) -> PathBuf {
        fs::canonicalize(std::env::temp_dir())
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!(
                "graphr-workspace-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
    }
}
