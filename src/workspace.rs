#[cfg(test)]
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::git::{
    CapturedSource, ChangeLayer, DependencyMode, Repository, WorktreeChanges, resolve_commit,
};
use crate::index::IndexStats;
use crate::store;

pub use crate::index::Engine;

pub(crate) const CACHE_FORMAT_VERSION: u32 = 6;
pub(crate) const GRAPH_ANALYZER_VERSION: u32 = 1;
pub(crate) const REVIEW_FORMAT_VERSION: u32 = 2;
const MANIFEST_SIZE_LIMIT: u64 = 64 * 1024;
const REVIEW_SIZE_LIMIT: u64 = 64 * 1024 * 1024;
static PRIVATE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
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

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct RootIdentity {
    pub repository_id: String,
    pub workspace_id: String,
    pub repository_root: PathBuf,
    pub worktree_root: PathBuf,
    pub git_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub index_path: PathBuf,
    pub object_format: String,
    pub branch: Option<String>,
    pub head_oid: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotTarget {
    Commit,
    Index,
    Worktree { include_untracked: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoChangeReason {
    IdenticalCommitOids,
    IdenticalTrees,
    EmptyIndexDelta,
    EmptyWorktreeDelta,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexRequest {
    pub worktree_root: PathBuf,
    pub base_ref: String,
    pub head_ref: String,
    pub target: SnapshotTarget,
    pub dependency_mode: DependencyMode,
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
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
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

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct IndexCompletion {
    pub snapshot_id: String,
    pub graph_image_id: String,
    pub provenance: Provenance,
    pub stats: IndexStats,
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
    pub provenance: Provenance,
    graph_checksum: String,
}

pub struct SnapshotCatalog {
    allowed_roots: Arc<AllowedRoots>,
    loaded: RwLock<HashMap<String, Arc<SnapshotEntry>>>,
    rejected: RwLock<HashMap<String, OperationError>>,
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
    graphs: PathBuf,
    reviews: PathBuf,
    snapshots: PathBuf,
    quarantine: PathBuf,
    tmp: PathBuf,
}

pub(crate) struct PrivateJob {
    root: RootIdentity,
    cache: CachePaths,
    path: PathBuf,
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
        self.cache.graphs.join(format!("{graph_image_id}.db"))
    }

    pub(crate) fn copy_seed(&self, source: &Path) -> Result<(), OperationError> {
        let mut source = open_regular(source, None).map_err(cache_corrupt)?;
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.graph_temp)
            .map_err(|error| cache_internal("cannot create private graph image", error))?;
        std::io::copy(&mut source, &mut target)
            .map_err(|error| cache_internal("cannot copy graph seed", error))?;
        target
            .sync_all()
            .map_err(|error| cache_internal("cannot sync graph seed", error))
    }
}

impl Drop for PrivateJob {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
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

    pub fn attach(&self, root: &RootIdentity) -> Result<(), OperationError> {
        self.allowed_roots.authorize(&root.worktree_root)?;
        let Some(cache) = cache_paths(root, false)? else {
            return Ok(());
        };
        let Some(directory) = existing_directory(&cache.snapshots)? else {
            return Ok(());
        };
        let mut manifests = fs::read_dir(directory)
            .map_err(|error| cache_internal("cannot read snapshot catalog", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| cache_internal("cannot read snapshot catalog", error))?;
        manifests.sort_by_key(fs::DirEntry::file_name);
        for manifest in manifests {
            let path = manifest.path();
            let Some(snapshot_id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".json"))
                .filter(|id| valid_id(id))
            else {
                continue;
            };
            match self.load_entry(root, &cache, snapshot_id, &path) {
                Ok(entry) => {
                    write_lock(&self.loaded).insert(snapshot_id.into(), entry);
                    write_lock(&self.rejected).remove(snapshot_id);
                }
                Err(error) => {
                    write_lock(&self.loaded).remove(snapshot_id);
                    write_lock(&self.rejected).insert(snapshot_id.into(), error);
                }
            }
        }
        Ok(())
    }

    pub fn get(&self, snapshot_id: &str) -> Result<Arc<SnapshotEntry>, OperationError> {
        if !valid_id(snapshot_id) {
            return Err(snapshot_not_found(snapshot_id));
        }
        if let Some(entry) = read_lock(&self.loaded).get(snapshot_id) {
            return Ok(entry.clone());
        }
        if let Some(error) = read_lock(&self.rejected).get(snapshot_id) {
            return Err(error.clone());
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

    pub(crate) fn begin(&self, root: &RootIdentity) -> Result<PrivateJob, OperationError> {
        self.allowed_roots.authorize(&root.worktree_root)?;
        let cache = cache_paths(root, true)?.expect("created above");
        let path = loop {
            let path = cache.tmp.join(private_name("job"));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(cache_internal("cannot create private job directory", error));
                }
            }
        };
        sync_directory(&cache.tmp)?;
        let capture_root = create_directory(&path, "capture")?;
        Ok(PrivateJob {
            root: root.clone(),
            cache,
            graph_temp: path.join("graph.db"),
            path,
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
        if let Some(error) = rejected {
            if !matches!(
                error.code,
                ErrorCode::CacheCorrupt | ErrorCode::SnapshotIncomplete
            ) {
                return Err(error);
            }
            let manifest = job.cache.snapshots.join(format!("{snapshot_id}.json"));
            quarantine_file(&job.cache, &manifest)?;
            rejected_path = Some(manifest.display().to_string());
            write_lock(&self.loaded).remove(snapshot_id);
            write_lock(&self.rejected).remove(snapshot_id);
        }

        let review = job.cache.reviews.join(format!("{review_id}.json"));
        match read_bounded(&review, REVIEW_SIZE_LIMIT) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(bytes)
                if bytes == expected_review
                    && blake3::hash(&bytes).to_hex().as_str() == review_id => {}
            Ok(_) | Err(_) => {
                quarantine_file(&job.cache, &review)?;
                rejected_path = Some(review.display().to_string());
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
                .filter(|(snapshot_id, error)| {
                    snapshot_id.as_str() != requested_snapshot_id
                        && matches!(
                            error.code,
                            ErrorCode::CacheCorrupt | ErrorCode::SnapshotIncomplete
                        )
                })
                .map(|(snapshot_id, _)| snapshot_id.clone())
                .collect::<Vec<_>>()
        };
        let mut rejected_path = None;
        for snapshot_id in rejected {
            let manifest = cache.snapshots.join(format!("{snapshot_id}.json"));
            quarantine_file(&cache, &manifest)?;
            rejected_path = Some(manifest.display().to_string());
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
        dependency_mode: DependencyMode,
        no_change_reason: Option<NoChangeReason>,
        mut provenance: Provenance,
    ) -> Result<Arc<SnapshotEntry>, OperationError> {
        if !valid_id(graph_image_id) || !valid_id(review_id) || !valid_id(&provenance.snapshot_id) {
            return Err(OperationError::new(
                ErrorCode::Internal,
                "generated cache identifier is invalid",
            ));
        }
        let review_temp = job.path.join("review.json");
        write_private(&review_temp, review_bytes)?;
        let review_path = job.cache.reviews.join(format!("{review_id}.json"));
        publish_no_replace(&review_temp, &review_path)?;
        let winner = read_bounded(&review_path, REVIEW_SIZE_LIMIT).map_err(cache_corrupt)?;
        if blake3::hash(&winner).to_hex().as_str() != review_id {
            return Err(cache_corrupt(
                "published review checksum does not match its ID",
            ));
        }

        let graph_path = job.graph_path(graph_image_id);
        if let Some(graph_temp) = graph_temp {
            fs::set_permissions(graph_temp, fs::Permissions::from_mode(0o444))
                .map_err(|error| cache_internal("cannot make graph image read-only", error))?;
            open_regular(graph_temp, None)
                .and_then(|file| file.sync_all())
                .map_err(|error| cache_internal("cannot sync read-only graph image", error))?;
            publish_no_replace(graph_temp, &graph_path)?;
        }
        let state = validate_published_image(&graph_path)?;
        let graph_checksum = hash_file(&graph_path).map_err(cache_corrupt)?;
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
        let manifest_temp = job.path.join("snapshot.json");
        write_private(&manifest_temp, &manifest_bytes)?;
        let manifest_path = job
            .cache
            .snapshots
            .join(format!("{}.json", manifest.provenance.snapshot_id));
        #[cfg(test)]
        before_manifest_publication(&PublicationPoint {
            snapshot_id: manifest.provenance.snapshot_id.clone(),
            graph_path: graph_path.clone(),
            review_path: review_path.clone(),
            manifest_path: manifest_path.clone(),
        })?;
        publish_no_replace(&manifest_temp, &manifest_path)?;
        let entry = self.load_entry(
            &job.root,
            &job.cache,
            &manifest.provenance.snapshot_id,
            &manifest_path,
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
        let graph = cache.graphs.join(format!("{graph_image_id}.db"));
        quarantine_file(&cache, &graph)?;
        let mut snapshot_ids = BTreeSet::from([requested_snapshot_id.to_owned()]);
        for entry in read_lock(&self.loaded).values() {
            if entry.graph_image_id == graph_image_id {
                snapshot_ids.insert(entry.provenance.snapshot_id.clone());
            }
        }
        for snapshot_id in &snapshot_ids {
            quarantine_file(&cache, &cache.snapshots.join(format!("{snapshot_id}.json")))?;
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
        manifest_path: &Path,
    ) -> Result<Arc<SnapshotEntry>, OperationError> {
        let bytes = match read_bounded(manifest_path, MANIFEST_SIZE_LIMIT) {
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
            .inspect(&manifest.provenance.worktree_root, &AtomicBool::new(false))?;
        if authorized.repository_id != manifest.provenance.repository_id
            || authorized.workspace_id != manifest.provenance.workspace_id
            || authorized.common_git_dir != manifest.provenance.common_git_dir
            || authorized.git_dir != manifest.provenance.git_dir
            || authorized.repository_root != manifest.provenance.repository_root
            || authorized.worktree_root != manifest.provenance.worktree_root
        {
            return Err(cache_corrupt("snapshot root identity changed"));
        }

        let review_path = cache.reviews.join(format!("{}.json", manifest.review_id));
        let review_bytes = match read_bounded(&review_path, REVIEW_SIZE_LIMIT) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(OperationError::new(
                    ErrorCode::SnapshotIncomplete,
                    "snapshot review is not published",
                ));
            }
            result => result.map_err(cache_corrupt)?,
        };
        if blake3::hash(&review_bytes).to_hex().as_str() != manifest.review_id {
            return Err(cache_corrupt("snapshot review checksum is invalid"));
        }
        let changes: WorktreeChanges = rmcp::serde_json::from_slice(&review_bytes)
            .map_err(|_| cache_corrupt("snapshot review is invalid"))?;
        if selected_layers(&changes) != manifest.provenance.selected_layers
            || changed_file_count(&changes) != manifest.provenance.changed_files
            || manifest.no_change_reason
                != expected_no_change_reason(
                    &changes,
                    &manifest.provenance.target_state,
                    &manifest.provenance.base_oid,
                    &manifest.provenance.head_oid,
                )
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
            },
            CACHE_FORMAT_VERSION,
            REVIEW_FORMAT_VERSION,
        );
        if recomputed != snapshot_id {
            return Err(cache_corrupt("snapshot ID does not match its manifest"));
        }

        let graph_path = cache.graphs.join(format!("{}.db", manifest.graph_image_id));
        match fs::symlink_metadata(&graph_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(OperationError::new(
                    ErrorCode::SnapshotIncomplete,
                    "snapshot graph is not published",
                ));
            }
            Err(error) => return Err(cache_corrupt(error)),
            Ok(_) => {}
        }
        if hash_file(&graph_path).map_err(cache_corrupt)? != manifest.graph_checksum
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
            provenance: manifest.provenance,
            graph_checksum: manifest.graph_checksum,
        }))
    }
}

fn cache_paths(root: &RootIdentity, create: bool) -> Result<Option<CachePaths>, OperationError> {
    validate_cache_root(root)?;
    let graphr = root.common_git_dir.join("graphr");
    let v6 = graphr.join("v6");
    if !create {
        let Some(_) = existing_directory(&graphr)? else {
            return Ok(None);
        };
        let Some(_) = existing_directory(&v6)? else {
            return Ok(None);
        };
    } else {
        create_directory(&root.common_git_dir, "graphr")?;
        create_directory(&graphr, "v6")?;
    }
    let paths = CachePaths {
        graphs: v6.join("graphs"),
        reviews: v6.join("reviews"),
        snapshots: v6.join("snapshots"),
        quarantine: v6.join("quarantine"),
        tmp: v6.join("tmp"),
    };
    for path in [
        &paths.graphs,
        &paths.reviews,
        &paths.snapshots,
        &paths.quarantine,
        &paths.tmp,
    ] {
        if create {
            create_directory(&v6, path.file_name().expect("fixed component"))?;
        } else {
            existing_directory(path)?;
        }
    }
    Ok(Some(paths))
}

fn validate_cache_root(root: &RootIdentity) -> Result<(), OperationError> {
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
    Ok(())
}

fn existing_directory(path: &Path) -> Result<Option<PathBuf>, OperationError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(cache_internal("cannot inspect cache directory", error)),
        Ok(metadata) if metadata.is_dir() => Ok(Some(path.to_path_buf())),
        Ok(_) => Err(
            OperationError::new(ErrorCode::CacheCorrupt, "cache path is not a directory")
                .with_path("path", path),
        ),
    }
}

fn create_directory(parent: &Path, name: impl AsRef<Path>) -> Result<PathBuf, OperationError> {
    let path = parent.join(name);
    match fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => sync_directory(parent)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(cache_internal("cannot create cache directory", error)),
    }
    existing_directory(&path)?.ok_or_else(|| {
        OperationError::new(ErrorCode::Internal, "cache directory disappeared")
            .with_path("path", &path)
    })
}

fn private_name(label: &str) -> String {
    format!(
        "{label}-{}-{}",
        std::process::id(),
        PRIVATE_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), OperationError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| cache_internal("cannot create private cache file", error))?;
    file.write_all(bytes)
        .map_err(|error| cache_internal("cannot write private cache file", error))?;
    file.sync_all()
        .map_err(|error| cache_internal("cannot sync private cache file", error))
}

fn publish_no_replace(temp: &Path, final_path: &Path) -> Result<bool, OperationError> {
    let metadata = fs::symlink_metadata(temp)
        .map_err(|error| cache_internal("cannot inspect private cache file", error))?;
    if !metadata.is_file() {
        return Err(OperationError::new(
            ErrorCode::Internal,
            "private cache path is not a regular file",
        ));
    }
    let published = match fs::hard_link(temp, final_path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(cache_internal("cannot publish cache file", error)),
    };
    fs::remove_file(temp)
        .map_err(|error| cache_internal("cannot remove private cache name", error))?;
    let temp_parent = temp.parent().expect("private file has parent");
    let final_parent = final_path.parent().expect("published file has parent");
    sync_directory(temp_parent)?;
    if final_parent != temp_parent {
        sync_directory(final_parent)?;
    }
    Ok(published)
}

fn quarantine_file(cache: &CachePaths, path: &Path) -> Result<(), OperationError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(cache_corrupt(error)),
        Ok(_) => {}
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cache");
    let target = loop {
        let target = cache
            .quarantine
            .join(format!("{name}.{}", private_name("rejected")));
        match fs::symlink_metadata(&target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break target,
            Err(error) => return Err(cache_corrupt(error)),
            Ok(_) => continue,
        }
    };
    fs::rename(path, &target)
        .map_err(|error| cache_corrupt(format!("cannot quarantine cache file: {error}")))?;
    sync_directory(path.parent().expect("cache file has parent"))?;
    sync_directory(&cache.quarantine)
}

fn sync_directory(path: &Path) -> Result<(), OperationError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| cache_internal("cannot sync cache directory", error))
}

fn open_regular(path: &Path, limit: Option<u64>) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || limit.is_some_and(|limit| metadata.len() > limit) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cache file is not a bounded regular file",
        ));
    }
    Ok(file)
}

fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut file = open_regular(path, Some(limit))?;
    let size = usize::try_from(file.metadata()?.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "cache file is too large")
    })?;
    let mut bytes = Vec::with_capacity(size);
    file.read_to_end(&mut bytes)?;
    if bytes.len() != size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cache file changed while reading",
        ));
    }
    Ok(bytes)
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = open_regular(path, None)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
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
) -> Result<crate::store::State, OperationError> {
    if hash_file(&entry.graph_path).map_err(cache_corrupt)? != entry.graph_checksum {
        return Err(cache_corrupt("snapshot graph checksum is invalid"));
    }
    validate_published_image(&entry.graph_path)
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
thread_local! {
    static BEFORE_MANIFEST_HOOK: RefCell<Option<PublicationHook>> = RefCell::new(None);
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

pub fn resolve_request(
    roots: &AllowedRoots,
    request: IndexRequest,
    cancelled: &AtomicBool,
) -> Result<ResolvedIndexRequest, OperationError> {
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
    })
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
        b"graphr.repository.v1",
        &[&repository.common_git_dir, &repository.object_format],
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
        index_path: repository.index_path,
        object_format: repository.object_format,
        branch: repository.branch,
        head_oid: repository.head_oid,
    }
}

pub(crate) fn graph_image_key(
    repository_id: &str,
    files: &[CapturedSource],
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
}

pub(crate) fn snapshot_key(
    input: &SnapshotKeyInput<'_>,
    cache_format_version: u32,
    review_format_version: u32,
) -> String {
    let mut hasher = blake3::Hasher::new();
    b"graphr.snapshot.v1"[..].hash_field(&mut hasher);
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use crate::git::{CapturedSource, DependencyMode, Language, SourceContent};
    use crate::index::Engine;
    use crate::store;

    use super::{
        AllowedRoots, ErrorCode, IndexRequest, OperationError, PublicationPoint, SnapshotCatalog,
        SnapshotKeyInput, SnapshotTarget, graph_image_key, resolve_request,
        set_before_manifest_hook_for_test, snapshot_key,
    };

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
        let key = graph_image_key("repository", &files, 6, 1, 4);
        assert_ne!(key, graph_image_key("other", &files, 6, 1, 4));
        assert_ne!(
            key,
            graph_image_key(
                "repository",
                &[source("src/lib.rs", "b", Language::Rust, "crate")],
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
                6,
                1,
                4,
            )
        );
        assert_ne!(key, graph_image_key("repository", &files, 7, 1, 4));
        assert_ne!(key, graph_image_key("repository", &files, 6, 2, 4));
        assert_ne!(key, graph_image_key("repository", &files, 6, 1, 5));
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
        fresh.attach(&identity).unwrap();
        assert_eq!(
            fresh.get(&completion.snapshot_id).unwrap().graph_image_id,
            completion.graph_image_id
        );
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

        catalog.attach(&identity).unwrap();

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
        std::env::temp_dir().join(format!(
            "graphr-workspace-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
