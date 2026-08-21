use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, FileTimes, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use crate::artifact::{AnalyzerKind, analyze, analyzer_kind};
use crate::workspace::{ErrorCode, NoChangeReason, OperationError, SnapshotTarget};

const STDOUT_LIMIT: usize = 64 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);
const SOURCE_LIMIT: u64 = 2 * 1024 * 1024;
const OVERSIZED_BLOB: &str = "Git blob exceeds the source size limit";

#[cfg(test)]
type GitTestHook = Box<dyn FnMut(&Path, &[&str], Option<&Path>) + Send>;

#[cfg(test)]
static GIT_TEST_HOOK: std::sync::Mutex<Option<GitTestHook>> = std::sync::Mutex::new(None);

pub struct Repository {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub common_git_dir_dev: u64,
    pub common_git_dir_ino: u64,
    pub index_path: PathBuf,
    pub branch: Option<String>,
    pub head_oid: String,
    pub object_format: String,
}

pub struct Source {
    pub path: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    serde::Deserialize,
    serde::Serialize,
    rmcp::schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum DependencyMode {
    #[default]
    Boundary,
    Full,
}

impl DependencyMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Boundary => "boundary",
            Self::Full => "full",
        }
    }
}

pub fn dependency_package(path: &str) -> Option<&str> {
    let (package, file) = path.strip_prefix(".cargo/vendor/")?.split_once('/')?;
    (!package.is_empty() && !file.is_empty()).then_some(package)
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            "javascript" => Some(Self::JavaScript),
            "typescript" => Some(Self::TypeScript),
            _ => None,
        }
    }
}

pub enum SourceContent {
    GitBlob(String),
    Captured {
        relative_path: PathBuf,
        digest: [u8; 32],
    },
}

pub struct CapturedSource {
    pub path: String,
    pub language: Language,
    pub git_oid: Option<String>,
    pub content_key: String,
    pub parse_context: String,
    pub content: SourceContent,
}

pub struct SourceSnapshot {
    pub capture_root: PathBuf,
    pub files: Vec<CapturedSource>,
    pub skipped: usize,
}

#[allow(dead_code)] // Task 4 consumes the review/provenance fields during publication.
pub struct SnapshotCapture {
    pub sources: SourceSnapshot,
    pub changes: WorktreeChanges,
    pub dirty_digest: String,
    pub commits_base_to_head: u64,
    pub changed_files: usize,
    pub no_change_reason: Option<NoChangeReason>,
}

pub(crate) struct BlobReader {
    child: Option<Child>,
    input: Option<BufWriter<ChildStdin>>,
    responses: mpsc::Receiver<Result<BlobResponse, String>>,
    response_thread: Option<thread::JoinHandle<()>>,
    stderr_thread: Option<thread::JoinHandle<Result<Vec<u8>, String>>>,
    overflow: mpsc::Receiver<()>,
}

struct BlobResponse {
    oid: String,
    content: Vec<u8>,
}

struct TargetInventory {
    sources: BTreeMap<String, (Language, InventoryContent)>,
    cargo_manifests: BTreeSet<String>,
    unmerged_paths: BTreeSet<String>,
    skipped: usize,
}

enum InventoryContent {
    GitBlob(String),
    Captured {
        relative_path: PathBuf,
        digest: [u8; 32],
    },
}

#[derive(Clone, Copy)]
enum InventoryKind {
    Source(Language),
    CargoManifest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct LineSpan {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub whole_file: bool,
    pub spans: Vec<LineSpan>,
    pub report_unmapped: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize)]
pub enum PathRecord {
    Deleted(String),
    Renamed(String, String),
    Untracked(String),
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
    Unmerged,
    Untracked,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    serde::Deserialize,
    serde::Serialize,
    rmcp::schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum ChangeLayer {
    Committed,
    Staged,
    Unstaged,
    Untracked,
}

impl ChangeLayer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::Untracked => "untracked",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ChangedPath {
    pub status: ChangeStatus,
    pub old_path: Option<String>,
    pub old_language: Option<Language>,
    pub path: String,
    pub language: Option<Language>,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub layers: Vec<ChangeLayer>,
}

pub fn changed_dependency_package(path: &ChangedPath) -> Option<&str> {
    let package = dependency_package(&path.path)?;
    if let Some(old) = path.old_path.as_deref()
        && dependency_package(old) != Some(package)
    {
        return None;
    }
    Some(package)
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct WorktreeChanges {
    pub files: Vec<ChangedFile>,
    pub records: Vec<PathRecord>,
    pub paths: Vec<ChangedPath>,
    pub source_patch: String,
    pub artifacts: ArtifactReview,
    pub skipped_paths: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOmission {
    Binary,
    InvalidUtf8,
    Oversized,
    NonRegular,
    TypeChanged,
    Unmerged,
}

impl ArtifactOmission {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::InvalidUtf8 => "invalid-utf8",
            Self::Oversized => "oversized",
            Self::NonRegular => "non-regular",
            Self::TypeChanged => "type-changed",
            Self::Unmerged => "unmerged",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ArtifactFile {
    pub path: String,
    pub analyzer: AnalyzerKind,
    pub diff_complete: bool,
    pub analysis_complete: bool,
    pub omission: Option<ArtifactOmission>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ArtifactReview {
    pub files: Vec<ArtifactFile>,
    pub analysis: String,
    pub patch: String,
}

impl ArtifactReview {
    pub fn file(&self, path: &str) -> Option<&ArtifactFile> {
        self.files
            .binary_search_by_key(&path, |file| file.path.as_str())
            .ok()
            .map(|index| &self.files[index])
    }

    pub fn is_complete(&self) -> bool {
        debug_assert!(
            self.files
                .iter()
                .all(|file| self.file(&file.path).is_some())
        );
        self.files
            .iter()
            .all(|file| file.diff_complete && file.analysis_complete)
    }

    pub fn analysis_complete(&self) -> bool {
        self.files.iter().all(|file| file.analysis_complete)
    }
}

#[derive(Default)]
struct TrackedArtifactSnapshot {
    review: ArtifactReview,
    stats: Vec<TrackedStat>,
    renames: Vec<(String, String)>,
}

#[derive(Clone, Copy)]
enum ArtifactSource {
    GitBlob,
    Worktree,
}

struct UntrackedSnapshot {
    paths: Vec<ChangedPath>,
    source_patch: Vec<u8>,
    artifacts: ArtifactReview,
    skipped_paths: usize,
}

impl WorktreeChanges {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.records.is_empty()
            && self.paths.is_empty()
            && self.source_patch.is_empty()
            && self.artifacts == ArtifactReview::default()
            && self.skipped_paths == 0
    }
}

impl Repository {
    pub fn discover_cancelled(path: &Path, cancelled: &AtomicBool) -> Result<Self, OperationError> {
        Self::discover(path, cancelled)
    }

    fn discover(path: &Path, cancelled: &AtomicBool) -> Result<Self, OperationError> {
        validate_discovery_path(path)?;
        let path = fs::canonicalize(path).map_err(|_| {
            OperationError::new(ErrorCode::RootUnknown, "requested root does not exist")
                .with_path("root", path)
        })?;
        if !path.is_dir() {
            return Err(OperationError::new(
                ErrorCode::RootUnknown,
                "requested root is not a directory",
            )
            .with_path("root", &path));
        }
        validate_discovery_path(&path)?;

        let root = parse_path(
            &run(
                &path,
                &["rev-parse", "--path-format=absolute", "--show-toplevel"],
                cancelled,
            )
            .map_err(git_metadata_error)?,
        )
        .map_err(git_metadata_error)?;
        let root = fs::canonicalize(root).map_err(|_| {
            OperationError::new(ErrorCode::GitMetadataInvalid, "cannot resolve Git root")
        })?;
        if path != root {
            return Err(OperationError::new(
                ErrorCode::RootNotWorktree,
                "requested root is not a Git worktree root",
            )
            .with_path("root", &path));
        }

        let git_dir = parse_path(
            &run(
                &root,
                &["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
                cancelled,
            )
            .map_err(git_metadata_error)?,
        )
        .map_err(git_metadata_error)?;
        let git_dir = fs::canonicalize(git_dir).map_err(|_| {
            OperationError::new(
                ErrorCode::GitMetadataInvalid,
                "cannot resolve Git directory",
            )
        })?;
        open_git_directory(&git_dir)?;
        let common_git_dir = parse_path(
            &run(
                &root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                cancelled,
            )
            .map_err(git_metadata_error)?,
        )
        .map_err(git_metadata_error)?;
        let common_git_dir = fs::canonicalize(common_git_dir).map_err(|_| {
            OperationError::new(
                ErrorCode::GitMetadataInvalid,
                "cannot resolve common Git directory",
            )
        })?;
        let common_git_dir_metadata = open_git_directory(&common_git_dir)?;
        if !git_dir.starts_with(&common_git_dir) {
            return Err(OperationError::new(
                ErrorCode::GitMetadataInvalid,
                "worktree Git directory is outside common Git directory",
            )
            .with_path("git_dir", &git_dir));
        }
        let index_path = parse_path(
            &run(
                &root,
                &["rev-parse", "--path-format=absolute", "--git-path", "index"],
                cancelled,
            )
            .map_err(git_metadata_error)?,
        )
        .map_err(git_metadata_error)?;
        let index_parent = index_path.parent().ok_or_else(|| {
            OperationError::new(
                ErrorCode::GitMetadataInvalid,
                "Git index path has no parent",
            )
        })?;
        let index_parent = fs::canonicalize(index_parent).map_err(|_| {
            OperationError::new(
                ErrorCode::GitMetadataInvalid,
                "cannot resolve Git index parent",
            )
        })?;
        if !index_parent.is_dir() || !index_parent.starts_with(&git_dir) {
            return Err(OperationError::new(
                ErrorCode::GitMetadataInvalid,
                "Git index parent is outside worktree Git directory",
            )
            .with_path("index_parent", &index_parent));
        }
        let index_path = index_parent.join(index_path.file_name().ok_or_else(|| {
            OperationError::new(
                ErrorCode::GitMetadataInvalid,
                "Git index path has no file name",
            )
        })?);
        let object_format = parse_value(
            &run(&root, &["rev-parse", "--show-object-format"], cancelled)
                .map_err(git_metadata_error)?,
        )
        .map_err(git_metadata_error)?;
        if !matches!(object_format.as_str(), "sha1" | "sha256") {
            return Err(OperationError::new(
                ErrorCode::GitMetadataInvalid,
                "Git returned an unsupported object format",
            ));
        }
        let branch = run_git(
            &root,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            true,
            false,
            Instant::now() + DEADLINE,
            STDOUT_LIMIT,
            None,
            cancelled,
        )
        .map_err(git_metadata_error)?;
        let branch = (!branch.is_empty())
            .then(|| parse_value(&branch))
            .transpose()
            .map_err(git_metadata_error)?;
        let head_oid = match run(
            &root,
            &["rev-parse", "--verify", "HEAD^{commit}"],
            cancelled,
        ) {
            Ok(output) => parse_value(&output).map_err(git_metadata_error)?,
            Err(error) if error.contains("cancelled") => {
                return Err(OperationError::new(
                    ErrorCode::JobCancelled,
                    "Git operation cancelled",
                ));
            }
            Err(_) => {
                return Err(OperationError::new(
                    ErrorCode::RefNotFound,
                    "HEAD does not name a commit",
                ));
            }
        };
        if !head_oid.is_empty() && !valid_oid(head_oid.as_bytes()) {
            return Err(OperationError::new(
                ErrorCode::GitMetadataInvalid,
                "Git returned an invalid HEAD object ID",
            ));
        }

        Ok(Self {
            root,
            git_dir,
            common_git_dir,
            common_git_dir_dev: common_git_dir_metadata.dev(),
            common_git_dir_ino: common_git_dir_metadata.ino(),
            index_path,
            branch,
            head_oid,
            object_format,
        })
    }

    pub fn capture_sources(
        &self,
        head_oid: &str,
        target: &SnapshotTarget,
        capture_root: &Path,
        cancelled: &AtomicBool,
    ) -> Result<SourceSnapshot, OperationError> {
        validate_capture_root(capture_root)?;
        if matches!(target, SnapshotTarget::Commit) && !valid_lower_oid(head_oid.as_bytes()) {
            return Err(OperationError::new(
                ErrorCode::InvalidParameters,
                "head object ID is invalid",
            ));
        }
        let inventory = match target {
            SnapshotTarget::Commit => {
                let output = run(
                    &self.root,
                    &["ls-tree", "-r", "-z", "--full-tree", head_oid],
                    cancelled,
                )
                .map_err(capture_error)?;
                parse_tree_inventory(&output).map_err(capture_error)?
            }
            SnapshotTarget::Index | SnapshotTarget::Worktree { .. } => {
                // ponytail: repeated index/digest samples reject ordinary races;
                // use a filesystem snapshot if adversarial ABA mutations matter.
                let copied_index = capture_index(
                    &self.index_path,
                    capture_root,
                    self.head_oid.is_empty(),
                    cancelled,
                )?;
                let output = run_with_index(
                    &self.root,
                    &["ls-files", "--stage", "-z"],
                    &copied_index,
                    cancelled,
                )
                .map_err(capture_error)?;
                let mut inventory = parse_index_inventory(&output).map_err(capture_error)?;
                if let SnapshotTarget::Worktree { include_untracked } = target {
                    overlay_worktree(
                        self,
                        &copied_index,
                        *include_untracked,
                        capture_root,
                        &mut inventory,
                        cancelled,
                    )?;
                }
                inventory
            }
        };

        Ok(source_snapshot(inventory, capture_root))
    }

    pub fn status_counts(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<(usize, usize, usize), OperationError> {
        let count = |args: &[&str]| {
            run(&self.root, args, cancelled)
                .map_err(capture_error)
                .map(|output| nul_records(&output).collect::<BTreeSet<_>>().len())
        };
        Ok((
            count(&["diff", "--cached", "--name-only", "-z", "HEAD"])?,
            count(&["diff", "--name-only", "-z"])?,
            count(&["ls-files", "--others", "--exclude-standard", "-z"])?,
        ))
    }

    #[allow(clippy::too_many_arguments)] // One call binds one fully resolved capture request.
    pub fn capture_snapshot(
        &self,
        base_oid: &str,
        head_oid: &str,
        target: &SnapshotTarget,
        dependency_mode: DependencyMode,
        capture_root: &Path,
        cancelled: &AtomicBool,
    ) -> Result<SnapshotCapture, OperationError> {
        validate_capture_root(capture_root)?;
        let mut capture_guard = CaptureDirectoryGuard::new(capture_root);
        if !valid_lower_oid(base_oid.as_bytes()) || !valid_lower_oid(head_oid.as_bytes()) {
            return Err(OperationError::new(
                ErrorCode::InvalidParameters,
                "base or head object ID is invalid",
            ));
        }
        let mutable = !matches!(target, SnapshotTarget::Commit);
        if mutable && resolve_commit(&self.root, "HEAD", "HEAD", cancelled)? != head_oid {
            return Err(OperationError::new(
                ErrorCode::HeadWorktreeMismatch,
                "head object ID does not match the worktree HEAD",
            ));
        }
        let commits_base_to_head = parse_count(
            &run(
                &self.root,
                &["rev-list", "--count", &format!("{base_oid}..{head_oid}")],
                cancelled,
            )
            .map_err(capture_error)?,
        )
        .map_err(capture_error)?;

        let (sources, mut changes, dirty_digest) = match target {
            SnapshotTarget::Commit => {
                let sources = self.capture_sources(head_oid, target, capture_root, cancelled)?;
                let changes = capture_target_changes(
                    self,
                    base_oid,
                    head_oid,
                    target,
                    None,
                    dependency_mode,
                    cancelled,
                )?;
                (
                    sources,
                    changes,
                    target_dirty_digest(self, target, None, &[], cancelled)?,
                )
            }
            SnapshotTarget::Index | SnapshotTarget::Worktree { .. } => {
                let copied_index = capture_index(
                    &self.index_path,
                    capture_root,
                    self.head_oid.is_empty(),
                    cancelled,
                )?;
                let index_inventory = run_with_index(
                    &self.root,
                    &["ls-files", "--stage", "-z"],
                    &copied_index,
                    cancelled,
                )
                .map_err(capture_error)?;
                let index_signature = run_with_index(
                    &self.root,
                    &["ls-files", "--stage", "-v", "-z"],
                    &copied_index,
                    cancelled,
                )
                .map_err(capture_error)?;
                let first_digest = target_dirty_digest(
                    self,
                    target,
                    Some(&copied_index),
                    &index_signature,
                    cancelled,
                )?;
                let mut inventory =
                    parse_index_inventory(&index_inventory).map_err(capture_error)?;
                if let SnapshotTarget::Worktree { include_untracked } = target {
                    overlay_worktree(
                        self,
                        &copied_index,
                        *include_untracked,
                        capture_root,
                        &mut inventory,
                        cancelled,
                    )?;
                }
                let sources = source_snapshot(inventory, capture_root);
                let changes = capture_target_changes(
                    self,
                    base_oid,
                    head_oid,
                    target,
                    Some(&copied_index),
                    dependency_mode,
                    cancelled,
                )?;
                let second_digest = target_dirty_digest(
                    self,
                    target,
                    Some(&copied_index),
                    &index_signature,
                    cancelled,
                )?;
                let current_index =
                    run(&self.root, &["ls-files", "--stage", "-v", "-z"], cancelled)
                        .map_err(capture_error)?;
                if first_digest != second_digest
                    || current_index != index_signature
                    || resolve_commit(&self.root, "HEAD", "HEAD", cancelled)? != head_oid
                {
                    return Err(OperationError::new(
                        ErrorCode::CaptureChanged,
                        "Git state changed during capture",
                    ));
                }
                (sources, changes, first_digest)
            }
        };
        assign_change_layers(
            self,
            base_oid,
            head_oid,
            target,
            mutable.then(|| capture_root.join("index")),
            &mut changes.paths,
            cancelled,
        )?;
        if mutable {
            let copied_index = capture_root.join("index");
            let index_signature = run_with_index(
                &self.root,
                &["ls-files", "--stage", "-v", "-z"],
                &copied_index,
                cancelled,
            )
            .map_err(capture_error)?;
            let final_digest = target_dirty_digest(
                self,
                target,
                Some(&copied_index),
                &index_signature,
                cancelled,
            )?;
            if final_digest != dirty_digest
                || run(&self.root, &["ls-files", "--stage", "-v", "-z"], cancelled)
                    .map_err(capture_error)?
                    != index_signature
                || resolve_commit(&self.root, "HEAD", "HEAD", cancelled)? != head_oid
            {
                return Err(OperationError::new(
                    ErrorCode::CaptureChanged,
                    "Git state changed during capture",
                ));
            }
        }
        let changed_files = changes
            .paths
            .iter()
            .map(|path| path.path.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let no_change_reason = changes.is_empty().then(|| match target {
            SnapshotTarget::Commit if base_oid == head_oid => NoChangeReason::IdenticalCommitOids,
            SnapshotTarget::Commit => NoChangeReason::IdenticalTrees,
            SnapshotTarget::Index => NoChangeReason::EmptyIndexDelta,
            SnapshotTarget::Worktree { .. } => NoChangeReason::EmptyWorktreeDelta,
        });
        let capture = SnapshotCapture {
            sources,
            changes,
            dirty_digest,
            commits_base_to_head,
            changed_files,
            no_change_reason,
        };
        capture_guard.retain();
        Ok(capture)
    }

    pub(crate) fn blob_reader(&self) -> Result<BlobReader, String> {
        BlobReader::spawn(&self.root)
    }
}

fn source_snapshot(inventory: TargetInventory, capture_root: &Path) -> SourceSnapshot {
    let mut files = inventory
        .sources
        .into_iter()
        .map(|(path, (language, content))| {
            let (git_oid, content_key, content) = match content {
                InventoryContent::GitBlob(oid) => {
                    (Some(oid.clone()), oid.clone(), SourceContent::GitBlob(oid))
                }
                InventoryContent::Captured {
                    relative_path,
                    digest,
                } => (
                    None,
                    blake3::Hash::from_bytes(digest).to_hex().to_string(),
                    SourceContent::Captured {
                        relative_path,
                        digest,
                    },
                ),
            };
            CapturedSource {
                path,
                language,
                git_oid,
                content_key,
                parse_context: String::new(),
                content,
            }
        })
        .collect::<Vec<_>>();
    crate::index::assign_parse_contexts(&mut files, &inventory.cargo_manifests);
    debug_assert!(files.windows(2).all(|pair| pair[0].path < pair[1].path));
    SourceSnapshot {
        capture_root: capture_root.to_owned(),
        files,
        skipped: inventory.skipped,
    }
}

impl BlobReader {
    fn spawn(root: &Path) -> Result<Self, String> {
        let mut command = git_command(root, false, None);
        command
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start Git blob reader: {error}"))?;
        let input = BufWriter::new(child.stdin.take().expect("piped stdin"));
        let output = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (response_tx, responses) = mpsc::channel();
        let response_thread = thread::spawn(move || {
            let mut output = BufReader::new(output);
            loop {
                let response = read_blob_response(&mut output);
                let stop = response.is_err();
                if response_tx.send(response).is_err() || stop {
                    break;
                }
            }
        });
        let (overflow_tx, overflow) = mpsc::channel();
        let stderr_thread = thread::spawn(move || {
            read_capped(stderr, STDERR_LIMIT, overflow_tx).map_err(|error| error.to_string())
        });
        Ok(Self {
            child: Some(child),
            input: Some(input),
            responses,
            response_thread: Some(response_thread),
            stderr_thread: Some(stderr_thread),
            overflow,
        })
    }

    pub(crate) fn read(
        &mut self,
        oid: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<Vec<u8>>, String> {
        if !valid_lower_oid(oid.as_bytes()) {
            return Err("invalid Git blob object ID".into());
        }
        check_cancelled(cancelled)?;
        let result = (|| {
            let input = self
                .input
                .as_mut()
                .ok_or_else(|| "Git blob reader is closed".to_owned())?;
            input
                .write_all(oid.as_bytes())
                .and_then(|()| input.write_all(b"\n"))
                .and_then(|()| input.flush())
                .map_err(|error| format!("cannot request Git blob: {error}"))?;
            let deadline = Instant::now() + DEADLINE;
            loop {
                check_cancelled(cancelled)?;
                if self.overflow.try_recv().is_ok() {
                    return Err("Git output exceeded its limit".into());
                }
                if Instant::now() >= deadline {
                    return Err("Git blob read timed out".into());
                }
                match self.responses.recv_timeout(Duration::from_millis(5)) {
                    Ok(Ok(response)) if response.oid == oid => return Ok(Some(response.content)),
                    Ok(Ok(_)) => return Err("Git returned the wrong blob object ID".into()),
                    Ok(Err(error)) if error == OVERSIZED_BLOB => return Ok(None),
                    Ok(Err(error)) => return Err(error),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("Git blob reader stopped".into());
                    }
                }
            }
        })();
        if !matches!(result, Ok(Some(_))) {
            self.shutdown();
        }
        result
    }

    fn shutdown(&mut self) {
        self.input.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.response_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn read_blob_response(output: &mut impl Read) -> Result<BlobResponse, String> {
    const HEADER_LIMIT: usize = 256;
    let mut header = Vec::with_capacity(64);
    loop {
        let mut byte = [0];
        output
            .read_exact(&mut byte)
            .map_err(|error| format!("cannot read Git blob header: {error}"))?;
        if byte[0] == b'\n' {
            break;
        }
        if header.len() == HEADER_LIMIT {
            return Err("Git blob header exceeded its limit".into());
        }
        header.push(byte[0]);
    }
    let fields = std::str::from_utf8(&header)
        .map_err(|_| "Git returned a malformed blob header".to_owned())?
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    if fields.len() != 3 || !valid_lower_oid(fields[0].as_bytes()) || fields[1] != "blob" {
        return Err("Git returned a malformed blob header".into());
    }
    let length = fields[2]
        .parse::<u64>()
        .ok()
        .filter(|length| *length <= SOURCE_LIMIT)
        .ok_or_else(|| OVERSIZED_BLOB.to_owned())?;
    let mut content = vec![0; length as usize];
    output
        .read_exact(&mut content)
        .map_err(|error| format!("cannot read Git blob: {error}"))?;
    let mut delimiter = [0];
    output
        .read_exact(&mut delimiter)
        .map_err(|error| format!("cannot read Git blob delimiter: {error}"))?;
    if delimiter != *b"\n" {
        return Err("Git returned a malformed blob delimiter".into());
    }
    Ok(BlobResponse {
        oid: fields[0].to_owned(),
        content,
    })
}

pub(crate) fn read_captured_source(
    capture_root: &Path,
    relative_path: &Path,
    digest: &[u8; 32],
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, String> {
    if relative_path.is_absolute()
        || !relative_path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err("captured source path is unsafe".into());
    }
    let path = relative_path
        .to_str()
        .ok_or_else(|| "captured source path is not valid UTF-8".to_owned())?;
    let content = read_regular_file(capture_root, path, SOURCE_LIMIT, cancelled)?
        .ok_or_else(|| "captured source is missing or invalid".to_owned())?;
    if blake3::hash(&content).as_bytes() != digest {
        return Err("captured source digest mismatch".into());
    }
    Ok(content)
}

fn validate_capture_root(path: &Path) -> Result<(), OperationError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        OperationError::new(
            ErrorCode::InvalidParameters,
            "capture directory does not exist",
        )
        .with_path("capture_root", path)
    })?;
    if !path.is_absolute()
        || !metadata.is_dir()
        || metadata.mode() & 0o077 != 0
        || fs::canonicalize(path).ok().as_deref() != Some(path)
    {
        return Err(OperationError::new(
            ErrorCode::InvalidParameters,
            "capture directory is not a private canonical directory",
        )
        .with_path("capture_root", path));
    }
    Ok(())
}

struct CaptureDirectoryGuard<'a> {
    path: &'a Path,
    retain: bool,
}

impl<'a> CaptureDirectoryGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            retain: false,
        }
    }

    fn retain(&mut self) {
        self.retain = true;
    }
}

impl Drop for CaptureDirectoryGuard<'_> {
    fn drop(&mut self) {
        if !self.retain {
            let _ = fs::remove_dir_all(self.path);
        }
    }
}

fn capture_index(
    index_path: &Path,
    capture_root: &Path,
    allow_missing: bool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, OperationError> {
    let parent = index_path.parent().ok_or_else(|| {
        OperationError::new(ErrorCode::GitMetadataInvalid, "Git index has no parent")
    })?;
    let name = index_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            OperationError::new(ErrorCode::GitMetadataInvalid, "Git index name is invalid")
        })?;
    let destination = capture_root.join("index");
    let before = match fs::symlink_metadata(index_path) {
        Ok(metadata) => metadata,
        Err(_) if allow_missing && !index_path.exists() => return Ok(destination),
        Err(_) => {
            return Err(OperationError::new(
                ErrorCode::CaptureChanged,
                "Git index cannot be captured",
            ));
        }
    };
    let content = match read_regular_file(parent, name, STDOUT_LIMIT as u64, cancelled)
        .map_err(capture_error)?
    {
        Some(content) => content,
        None => {
            return Err(OperationError::new(
                ErrorCode::CaptureChanged,
                "Git index cannot be captured",
            ));
        }
    };
    let after = fs::symlink_metadata(index_path).map_err(|_| {
        OperationError::new(ErrorCode::CaptureChanged, "Git index cannot be captured")
    })?;
    if !same_file_version(&before, &after) {
        return Err(OperationError::new(
            ErrorCode::CaptureChanged,
            "Git index changed during capture",
        ));
    }
    let modified = before.modified().map_err(|_| {
        OperationError::new(
            ErrorCode::GitMetadataInvalid,
            "Git index modification time is invalid",
        )
    })?;
    write_private_file(&destination, &content, Some(modified)).map_err(capture_error)?;
    Ok(destination)
}

fn overlay_worktree(
    repository: &Repository,
    copied_index: &Path,
    include_untracked: bool,
    capture_root: &Path,
    inventory: &mut TargetInventory,
    cancelled: &AtomicBool,
) -> Result<(), OperationError> {
    let dirty = run_with_index(
        &repository.root,
        &["ls-files", "--modified", "--deleted", "-z"],
        copied_index,
        cancelled,
    )
    .map_err(capture_error)?;
    let (dirty, dirty_skipped) = parse_inventory_paths(&dirty).map_err(capture_error)?;
    inventory.skipped += dirty_skipped;
    let untracked = if include_untracked {
        let (paths, skipped) = parse_inventory_paths(
            &run_with_index(
                &repository.root,
                &["ls-files", "--others", "--exclude-standard", "-z"],
                copied_index,
                cancelled,
            )
            .map_err(capture_error)?,
        )
        .map_err(capture_error)?;
        inventory.skipped += skipped;
        paths
    } else {
        BTreeSet::new()
    };
    let selected = dirty
        .iter()
        .chain(untracked.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    let capture_directory = capture_root.join("sources");
    if selected
        .iter()
        .any(|path| language_for_path(path).is_some())
    {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&capture_directory)
            .map_err(|error| capture_error(format!("cannot create capture directory: {error}")))?;
    }
    for (ordinal, path) in selected.iter().enumerate() {
        check_cancelled(cancelled).map_err(capture_error)?;
        if inventory.unmerged_paths.contains(path) {
            continue;
        }
        let Some(kind) = inventory_kind(path) else {
            continue;
        };
        match kind {
            InventoryKind::CargoManifest => {
                if safe_regular_metadata(&repository.root, path).is_some() {
                    inventory.cargo_manifests.insert(path.clone());
                } else {
                    inventory.cargo_manifests.remove(path);
                }
            }
            InventoryKind::Source(language) => {
                inventory.sources.remove(path);
                match read_regular_file(&repository.root, path, SOURCE_LIMIT, cancelled)
                    .map_err(capture_error)?
                {
                    Some(content) => {
                        let digest = *blake3::hash(&content).as_bytes();
                        let relative_path =
                            PathBuf::from("sources").join(format!("{ordinal:016x}"));
                        write_private_file(&capture_root.join(&relative_path), &content, None)
                            .map_err(capture_error)?;
                        inventory.sources.insert(
                            path.clone(),
                            (
                                language,
                                InventoryContent::Captured {
                                    relative_path,
                                    digest,
                                },
                            ),
                        );
                    }
                    None => {
                        if fs::symlink_metadata(repository.root.join(path)).is_ok() {
                            inventory.skipped += 1;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn write_private_file(
    path: &Path,
    content: &[u8],
    modified: Option<std::time::SystemTime>,
) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("cannot create private capture file: {error}"))?;
    file.write_all(content)
        .map_err(|error| format!("cannot write private capture file: {error}"))?;
    if let Some(modified) = modified {
        file.set_times(FileTimes::new().set_modified(modified))
            .map_err(|error| format!("cannot write private capture time: {error}"))?;
    }
    file.sync_all()
        .map_err(|error| format!("cannot sync private capture file: {error}"))
}

fn capture_error(error: String) -> OperationError {
    if error.contains("cancelled") {
        OperationError::new(ErrorCode::JobCancelled, "source capture cancelled")
    } else if error.contains("changed while reading") || error.contains("inventories disagree") {
        OperationError::new(ErrorCode::CaptureChanged, "source changed during capture")
    } else if error.starts_with("cannot create private")
        || error.starts_with("cannot write private")
        || error.starts_with("cannot sync private")
        || error.starts_with("cannot create capture")
    {
        OperationError::new(ErrorCode::Internal, error)
    } else {
        OperationError::new(ErrorCode::GitMetadataInvalid, error)
    }
}

fn parse_count(output: &[u8]) -> Result<u64, String> {
    parse_value(output)?
        .parse()
        .map_err(|_| "Git returned an invalid count".to_owned())
}

#[allow(clippy::too_many_arguments)] // The shared target selector keeps all diff streams aligned.
fn capture_target_changes(
    repository: &Repository,
    base_oid: &str,
    head_oid: &str,
    target: &SnapshotTarget,
    index_file: Option<&Path>,
    dependency_mode: DependencyMode,
    cancelled: &AtomicBool,
) -> Result<WorktreeChanges, OperationError> {
    let inventory = run_final_diff(
        repository,
        base_oid,
        head_oid,
        target,
        index_file,
        &[
            "--raw",
            "-z",
            "--abbrev=64",
            "--no-renames",
            "--diff-filter=AMDTU",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=none",
        ],
        &[],
        cancelled,
    )?;
    let mut source_paths = vec!["*.rs", "*.py"];
    if dependency_mode == DependencyMode::Boundary {
        source_paths.push(":(glob,exclude).cargo/vendor/*/**");
    }
    let tracked = run_final_diff(
        repository,
        base_oid,
        head_oid,
        target,
        index_file,
        &[
            "--raw",
            "-z",
            "--patch",
            "--unified=0",
            "--abbrev=64",
            "--find-renames=50%",
            "-l0",
            "--diff-filter=AMDR",
            "--diff-algorithm=myers",
            "--no-indent-heuristic",
            "-O/dev/null",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--text",
        ],
        &source_paths,
        cancelled,
    )?;
    let mut artifact_paths = vec![".", ":(exclude)*.rs", ":(exclude)*.py"];
    if dependency_mode == DependencyMode::Boundary {
        artifact_paths.push(":(glob,exclude).cargo/vendor/*/**");
    }
    let artifact_output = run_final_diff(
        repository,
        base_oid,
        head_oid,
        target,
        index_file,
        &[
            "--raw",
            "-z",
            "--patch",
            "--unified=0",
            "--abbrev=64",
            "--find-renames=50%",
            "-l0",
            "--diff-filter=AMDR",
            "--diff-algorithm=myers",
            "--no-indent-heuristic",
            "-O/dev/null",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--text",
        ],
        &artifact_paths,
        cancelled,
    )?;
    let untracked = if matches!(
        target,
        SnapshotTarget::Worktree {
            include_untracked: true
        }
    ) {
        run_with_index(
            &repository.root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
            index_file.expect("worktree target has a captured index"),
            cancelled,
        )
        .map_err(capture_error)?
    } else {
        Vec::new()
    };
    let tracked = parse_tracked_changes(&tracked, cancelled).map_err(capture_error)?;
    let artifact_source = if matches!(target, SnapshotTarget::Worktree { .. }) {
        ArtifactSource::Worktree
    } else {
        ArtifactSource::GitBlob
    };
    let artifacts = capture_tracked_artifacts(
        &repository.root,
        &artifact_output,
        artifact_source,
        cancelled,
    )
    .map_err(capture_error)?;
    let inventory = parse_change_inventory(&inventory, cancelled).map_err(capture_error)?;
    let untracked = capture_untracked(
        &repository.root,
        &untracked,
        dependency_mode,
        true,
        cancelled,
    )
    .map_err(capture_error)?;
    merge_changes(
        tracked,
        artifacts,
        inventory,
        untracked,
        dependency_mode,
        cancelled,
    )
    .map_err(capture_error)
}

#[allow(clippy::too_many_arguments)]
fn run_final_diff(
    repository: &Repository,
    base_oid: &str,
    head_oid: &str,
    target: &SnapshotTarget,
    index_file: Option<&Path>,
    options: &[&str],
    paths: &[&str],
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, OperationError> {
    let mut args = vec!["diff"];
    args.extend_from_slice(options);
    match target {
        SnapshotTarget::Commit => {
            args.push(base_oid);
            args.push(head_oid);
        }
        SnapshotTarget::Index => {
            args.push("--cached");
            args.push(base_oid);
        }
        SnapshotTarget::Worktree { .. } => args.push(base_oid),
    }
    if !paths.is_empty() {
        args.push("--");
        args.extend_from_slice(paths);
    }
    run_git(
        &repository.root,
        &args,
        false,
        false,
        Instant::now() + DEADLINE,
        STDOUT_LIMIT,
        index_file,
        cancelled,
    )
    .map_err(capture_error)
}

fn assign_change_layers(
    repository: &Repository,
    base_oid: &str,
    head_oid: &str,
    target: &SnapshotTarget,
    index_file: Option<PathBuf>,
    paths: &mut [ChangedPath],
    cancelled: &AtomicBool,
) -> Result<(), OperationError> {
    let options = [
        "--raw",
        "-z",
        "--abbrev=64",
        "--find-renames=50%",
        "-l0",
        "--diff-filter=AMDRTU",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--ignore-submodules=none",
    ];
    let mut inventories = vec![(
        ChangeLayer::Committed,
        parse_change_inventory(
            &run_final_diff(
                repository,
                base_oid,
                head_oid,
                &SnapshotTarget::Commit,
                None,
                &options,
                &[],
                cancelled,
            )?,
            cancelled,
        )
        .map_err(capture_error)?
        .0,
    )];
    if !matches!(target, SnapshotTarget::Commit) {
        let index_file = index_file
            .as_deref()
            .expect("mutable target has a captured index");
        inventories.push((
            ChangeLayer::Staged,
            parse_change_inventory(
                &run_final_diff(
                    repository,
                    head_oid,
                    head_oid,
                    &SnapshotTarget::Index,
                    Some(index_file),
                    &options,
                    &[],
                    cancelled,
                )?,
                cancelled,
            )
            .map_err(capture_error)?
            .0,
        ));
        if matches!(target, SnapshotTarget::Worktree { .. }) {
            let mut args = vec!["diff"];
            args.extend_from_slice(&options);
            let output = run_git(
                &repository.root,
                &args,
                false,
                false,
                Instant::now() + DEADLINE,
                STDOUT_LIMIT,
                Some(index_file),
                cancelled,
            )
            .map_err(capture_error)?;
            inventories.push((
                ChangeLayer::Unstaged,
                parse_change_inventory(&output, cancelled)
                    .map_err(capture_error)?
                    .0,
            ));
        }
    }
    apply_layers(paths, &inventories);
    Ok(())
}

fn same_change_path(final_path: &ChangedPath, layer_path: &ChangedPath) -> bool {
    [
        Some(final_path.path.as_str()),
        final_path.old_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|final_endpoint| {
        [
            Some(layer_path.path.as_str()),
            layer_path.old_path.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|layer_endpoint| final_endpoint == layer_endpoint)
    })
}

fn apply_layers(paths: &mut [ChangedPath], inventories: &[(ChangeLayer, Vec<ChangedPath>)]) {
    for path in paths {
        path.layers.clear();
        for (layer, inventory) in inventories {
            if inventory
                .iter()
                .any(|change| same_change_path(path, change))
            {
                path.layers.push(*layer);
            }
        }
        if path.status == ChangeStatus::Untracked {
            path.layers.push(ChangeLayer::Untracked);
        }
        path.layers.sort_unstable();
        path.layers.dedup();
    }
}

fn target_dirty_digest(
    repository: &Repository,
    target: &SnapshotTarget,
    index_file: Option<&Path>,
    index_signature: &[u8],
    cancelled: &AtomicBool,
) -> Result<String, OperationError> {
    let mut hash = blake3::Hasher::new();
    hash_field(&mut hash, b"domain", b"graphr-dirty-v1");
    match target {
        SnapshotTarget::Commit => {
            hash_field(&mut hash, b"target", b"commit");
            hash_field(&mut hash, b"include_untracked", b"false");
            hash_field(&mut hash, b"overlay", b"empty");
        }
        SnapshotTarget::Index => {
            hash_field(&mut hash, b"target", b"index");
            hash_field(&mut hash, b"include_untracked", b"false");
            hash_field(&mut hash, b"index", index_signature);
        }
        SnapshotTarget::Worktree { include_untracked } => {
            hash_field(&mut hash, b"target", b"worktree");
            hash_field(
                &mut hash,
                b"include_untracked",
                if *include_untracked {
                    b"true"
                } else {
                    b"false"
                },
            );
            hash_field(&mut hash, b"index", index_signature);
            let index_file = index_file.expect("worktree target has a captured index");
            let mut selected = nul_records(
                &run_with_index(
                    &repository.root,
                    &["ls-files", "--modified", "--deleted", "-z"],
                    index_file,
                    cancelled,
                )
                .map_err(capture_error)?,
            )
            .map(<[u8]>::to_vec)
            .collect::<BTreeSet<_>>();
            if *include_untracked {
                selected.extend(
                    nul_records(
                        &run_with_index(
                            &repository.root,
                            &["ls-files", "--others", "--exclude-standard", "-z"],
                            index_file,
                            cancelled,
                        )
                        .map_err(capture_error)?,
                    )
                    .map(<[u8]>::to_vec),
                );
            }
            for path in selected {
                hash_dirty_path(&mut hash, &repository.root, &path, cancelled)?;
            }
        }
    }
    Ok(hash.finalize().to_hex().to_string())
}

fn hash_dirty_path(
    hash: &mut blake3::Hasher,
    root: &Path,
    raw_path: &[u8],
    cancelled: &AtomicBool,
) -> Result<(), OperationError> {
    let relative = Path::new(OsStr::from_bytes(raw_path));
    if relative.is_absolute()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(OperationError::new(
            ErrorCode::GitMetadataInvalid,
            "Git returned an unsafe dirty path",
        ));
    }
    check_cancelled(cancelled).map_err(capture_error)?;
    hash_field(hash, b"path", raw_path);
    let candidate = root.join(relative);
    let Ok(before) = fs::symlink_metadata(&candidate) else {
        hash_field(hash, b"type", b"absent");
        hash_field(hash, b"mode", &0_u32.to_le_bytes());
        hash_field(hash, b"size", &0_u64.to_le_bytes());
        hash_field(hash, b"bytes", &[]);
        return Ok(());
    };
    let kind = if before.is_file() {
        b"regular".as_slice()
    } else if before.file_type().is_symlink() {
        b"symlink".as_slice()
    } else if before.is_dir() {
        b"directory".as_slice()
    } else {
        b"other".as_slice()
    };
    hash_field(hash, b"type", kind);
    hash_field(hash, b"mode", &before.mode().to_le_bytes());
    hash_field(hash, b"size", &before.len().to_le_bytes());
    if before.is_file() {
        if fs::canonicalize(&candidate).ok().as_deref() != Some(candidate.as_path()) {
            return Err(OperationError::new(
                ErrorCode::CaptureChanged,
                "dirty file cannot be read safely",
            ));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&candidate)
            .map_err(|_| {
                OperationError::new(ErrorCode::CaptureChanged, "dirty file cannot be opened")
            })?;
        let opened = file.metadata().map_err(|_| {
            OperationError::new(ErrorCode::CaptureChanged, "dirty file cannot be inspected")
        })?;
        if !same_file_version(&before, &opened) {
            return Err(OperationError::new(
                ErrorCode::CaptureChanged,
                "dirty file changed during capture",
            ));
        }
        hash.update(&(b"bytes".len() as u64).to_le_bytes());
        hash.update(b"bytes");
        hash.update(&opened.len().to_le_bytes());
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            check_cancelled(cancelled).map_err(capture_error)?;
            let read = file.read(&mut buffer).map_err(|_| {
                OperationError::new(ErrorCode::CaptureChanged, "dirty file cannot be read")
            })?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        let after = fs::symlink_metadata(&candidate).map_err(|_| {
            OperationError::new(ErrorCode::CaptureChanged, "dirty file disappeared")
        })?;
        if !same_file_version(&opened, &after) {
            return Err(OperationError::new(
                ErrorCode::CaptureChanged,
                "dirty file changed during capture",
            ));
        }
    } else if before.file_type().is_symlink() {
        let value = fs::read_link(&candidate).map_err(|_| {
            OperationError::new(ErrorCode::CaptureChanged, "dirty link cannot be read")
        })?;
        hash_field(hash, b"bytes", value.as_os_str().as_bytes());
        let after = fs::symlink_metadata(&candidate).map_err(|_| {
            OperationError::new(ErrorCode::CaptureChanged, "dirty link disappeared")
        })?;
        if !same_file_version(&before, &after) {
            return Err(OperationError::new(
                ErrorCode::CaptureChanged,
                "dirty link changed during capture",
            ));
        }
    } else {
        hash_field(hash, b"bytes", &[]);
        let after = fs::symlink_metadata(&candidate).map_err(|_| {
            OperationError::new(ErrorCode::CaptureChanged, "dirty path disappeared")
        })?;
        if !same_file_version(&before, &after) {
            return Err(OperationError::new(
                ErrorCode::CaptureChanged,
                "dirty path changed during capture",
            ));
        }
    }
    Ok(())
}

fn hash_field(hash: &mut blake3::Hasher, label: &[u8], value: &[u8]) {
    hash.update(&(label.len() as u64).to_le_bytes());
    hash.update(label);
    hash.update(&(value.len() as u64).to_le_bytes());
    hash.update(value);
}

fn read_regular_file(
    root: &Path,
    path: &str,
    limit: u64,
    cancelled: &AtomicBool,
) -> Result<Option<Vec<u8>>, String> {
    check_cancelled(cancelled)?;
    let candidate = root.join(path);
    let Ok(before) = fs::symlink_metadata(&candidate) else {
        return Ok(None);
    };
    if !before.is_file() || before.len() > limit {
        return Ok(None);
    }
    let Ok(canonical) = fs::canonicalize(&candidate) else {
        return Ok(None);
    };
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&candidate)
    else {
        return Ok(None);
    };
    let Ok(after) = file.metadata() else {
        return Ok(None);
    };
    if canonical != candidate
        || !after.is_file()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || after.len() > limit
    {
        return Ok(None);
    }
    let mut content = Vec::with_capacity(after.len() as usize);
    let Ok(_) = Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut content)
    else {
        return Ok(None);
    };
    let finished = file
        .metadata()
        .map_err(|error| format!("cannot recheck file {path}: {error}"))?;
    let current = fs::symlink_metadata(&candidate)
        .map_err(|error| format!("cannot recheck file {path}: {error}"))?;
    if !current.is_file()
        || !same_file_version(&before, &finished)
        || !same_file_version(&finished, &current)
    {
        return Err(format!("file changed while reading: {path}"));
    }
    check_cancelled(cancelled)?;
    if content.len() as u64 > limit {
        Ok(None)
    } else {
        Ok(Some(content))
    }
}

fn capture_untracked(
    root: &Path,
    input: &[u8],
    dependency_mode: DependencyMode,
    include_patch: bool,
    cancelled: &AtomicBool,
) -> Result<UntrackedSnapshot, String> {
    if !input.is_empty() && !input.ends_with(&[0]) {
        return Err("Git returned malformed untracked paths".into());
    }
    let mut hash = blake3::Hasher::new();
    hash.update(&(input.len() as u64).to_le_bytes());
    hash.update(input);
    let mut paths = Vec::new();
    let mut source_patch = Vec::new();
    let mut artifacts = ArtifactReview::default();
    let mut analysis = Vec::new();
    let mut output_len = 0usize;
    let mut skipped_paths = 0;
    // ponytail: one Git process per eligible untracked file, all sharing one
    // deadline; batch only if large untracked source sets become routine.
    let patch_deadline = Instant::now() + DEADLINE;
    if input.is_empty() {
        return Ok(UntrackedSnapshot {
            paths,
            source_patch,
            artifacts,
            skipped_paths,
        });
    }

    for (index, raw_path) in input
        .strip_suffix(&[0])
        .expect("validated trailing NUL")
        .split(|byte| *byte == 0)
        .enumerate()
    {
        check_progress(index, cancelled)?;
        if raw_path.is_empty() {
            return Err("Git returned malformed untracked paths".into());
        }
        let Some(path) = parse_change_path(raw_path)? else {
            skipped_paths += 1;
            continue;
        };
        let candidate = root.join(&path);
        let before = fs::symlink_metadata(&candidate).ok();
        let before_regular = before.as_ref().is_some_and(|metadata| {
            metadata.is_file()
                && fs::canonicalize(&candidate).is_ok_and(|canonical| canonical == candidate)
        });
        let boundary_dependency =
            dependency_mode == DependencyMode::Boundary && dependency_package(&path).is_some();
        let content = if !boundary_dependency
            && before_regular
            && before
                .as_ref()
                .is_some_and(|metadata| metadata.len() <= SOURCE_LIMIT)
        {
            read_regular_file(root, &path, SOURCE_LIMIT, cancelled)?
        } else {
            None
        };
        let after = fs::symlink_metadata(&candidate).ok();
        let after_regular = after.as_ref().is_some_and(|metadata| {
            metadata.is_file()
                && fs::canonicalize(&candidate).is_ok_and(|canonical| canonical == candidate)
        });
        match (&before, &after) {
            (Some(before), Some(after))
                if same_file_version(before, after) && before_regular == after_regular => {}
            (None, None) => {}
            _ => return Err(format!("file changed while reading: {path}")),
        }
        let regular = before_regular && after_regular;
        if let Some(metadata) = &after {
            hash_file_version(&mut hash, metadata);
        }
        let language = language_for_path(&path);
        let mut reported_language = regular.then_some(language).flatten();
        let (mut additions, mut deletions) = (None, None);
        let mut artifact = None;
        if boundary_dependency {
            hash.update(b"boundary");
        } else if !regular
            || content.is_none() && after.as_ref().is_some_and(|m| m.len() <= SOURCE_LIMIT)
        {
            hash.update(ArtifactOmission::NonRegular.as_str().as_bytes());
            artifact = Some(ArtifactFile {
                path: path.clone(),
                analyzer: analyzer_kind(&path),
                diff_complete: false,
                analysis_complete: false,
                omission: Some(ArtifactOmission::NonRegular),
            });
        } else if after
            .as_ref()
            .is_some_and(|metadata| metadata.len() > SOURCE_LIMIT)
        {
            hash.update(ArtifactOmission::Oversized.as_str().as_bytes());
            if language.is_none() {
                artifact = Some(ArtifactFile {
                    path: path.clone(),
                    analyzer: analyzer_kind(&path),
                    diff_complete: false,
                    analysis_complete: false,
                    omission: Some(ArtifactOmission::Oversized),
                });
            }
        } else if let Some(content) = content {
            hash.update(&(content.len() as u64).to_le_bytes());
            hash.update(&content);
            match semantic_text(content) {
                Err(reason) => {
                    reported_language = None;
                    hash.update(reason.as_str().as_bytes());
                    artifact = Some(ArtifactFile {
                        path: path.clone(),
                        analyzer: analyzer_kind(&path),
                        diff_complete: false,
                        analysis_complete: false,
                        omission: Some(reason),
                    });
                }
                Ok(text) => {
                    let file_patch = untracked_patch(root, &path, patch_deadline, cancelled)?;
                    let current = safe_regular_metadata(root, &path)
                        .ok_or_else(|| format!("file changed while reading: {path}"))?;
                    if !same_file_version(
                        after.as_ref().expect("regular file has metadata"),
                        &current,
                    ) {
                        return Err(format!("file changed while reading: {path}"));
                    }
                    let parsed = parse_patch_hunks(&file_patch, 1, cancelled)?;
                    additions = Some(parsed[0].additions);
                    deletions = Some(parsed[0].deletions);
                    output_len = output_len.saturating_add(file_patch.len());
                    hash.update(&(file_patch.len() as u64).to_le_bytes());
                    hash.update(&file_patch);
                    if let Some(language) = language {
                        hash.update(language.as_str().as_bytes());
                        if include_patch {
                            source_patch.extend_from_slice(&file_patch);
                        }
                    } else {
                        let output_count = analysis.len();
                        record_artifact_analysis(
                            &path,
                            None,
                            Some(&text),
                            &mut hash,
                            &mut analysis,
                        )?;
                        if analysis.len() > output_count {
                            output_len = output_len
                                .saturating_add(analysis.last().expect("analysis added").len())
                                .saturating_add(usize::from(output_count > 0));
                        }
                        if include_patch {
                            artifacts.patch.push_str(
                                std::str::from_utf8(&file_patch)
                                    .expect("UTF-8 content produces a UTF-8 patch"),
                            );
                        }
                        artifact = Some(ArtifactFile {
                            path: path.clone(),
                            analyzer: analyzer_kind(&path),
                            diff_complete: true,
                            analysis_complete: true,
                            omission: None,
                        });
                    }
                }
            }
        } else {
            hash.update(&u64::MAX.to_le_bytes());
        }
        if output_len > STDOUT_LIMIT {
            return Err("Git output exceeded its limit".into());
        }
        if let Some(artifact) = artifact {
            hash.update(artifact.analyzer.as_str().as_bytes());
            artifacts.files.push(artifact);
        }
        paths.push(ChangedPath {
            status: ChangeStatus::Untracked,
            old_path: None,
            old_language: None,
            path: path.clone(),
            language: reported_language,
            additions,
            deletions,
            layers: vec![ChangeLayer::Untracked],
        });
    }
    artifacts
        .files
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    analysis.sort_unstable();
    if include_patch {
        artifacts.analysis = analysis.join("\n");
    }
    Ok(UntrackedSnapshot {
        paths,
        source_patch,
        artifacts,
        skipped_paths,
    })
}

fn untracked_patch(
    root: &Path,
    path: &str,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let mut patch = run_git(
        root,
        &[
            "diff",
            "--no-index",
            "--unified=0",
            "--abbrev=64",
            "--no-renames",
            "--diff-algorithm=myers",
            "--no-indent-heuristic",
            "-O/dev/null",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--text",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--",
            "/dev/null",
            path,
        ],
        true,
        true,
        deadline,
        STDOUT_LIMIT,
        None,
        cancelled,
    )?;
    // A no-index diff outside the repository always hashes with SHA-1. The
    // header is not needed for review hunks, so omit it rather than claim the
    // wrong object format for SHA-256 repositories.
    if let Some(start) = patch.windows(7).position(|window| window == b"\nindex ") {
        let start = start + 1;
        if let Some(end) = patch[start..].iter().position(|byte| *byte == b'\n') {
            patch.drain(start..start + end + 1);
        }
    }
    Ok(patch)
}

fn safe_regular_metadata(root: &Path, path: &str) -> Option<fs::Metadata> {
    let candidate = root.join(path);
    let metadata = fs::symlink_metadata(&candidate).ok()?;
    (metadata.is_file()
        && fs::canonicalize(&candidate).is_ok_and(|canonical| canonical == candidate))
    .then_some(metadata)
}

fn hash_file_version(hash: &mut blake3::Hasher, metadata: &fs::Metadata) {
    hash.update(&metadata.mode().to_le_bytes());
    hash.update(&metadata.dev().to_le_bytes());
    hash.update(&metadata.ino().to_le_bytes());
    hash.update(&metadata.len().to_le_bytes());
    hash.update(&metadata.mtime().to_le_bytes());
    hash.update(&metadata.mtime_nsec().to_le_bytes());
    hash.update(&metadata.ctime().to_le_bytes());
    hash.update(&metadata.ctime_nsec().to_le_bytes());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
    Unmerged,
}

impl RawKind {
    const fn status(self) -> ChangeStatus {
        match self {
            Self::Added => ChangeStatus::Added,
            Self::Modified => ChangeStatus::Modified,
            Self::Deleted => ChangeStatus::Deleted,
            Self::Renamed => ChangeStatus::Renamed,
            Self::TypeChanged => ChangeStatus::TypeChanged,
            Self::Unmerged => ChangeStatus::Unmerged,
        }
    }
}

struct RawChange {
    kind: RawKind,
    old: Option<String>,
    new: Option<String>,
    old_oid: Option<String>,
    new_oid: Option<String>,
    old_regular: bool,
    new_regular: bool,
}

struct RawHeader {
    kind: RawKind,
    old_oid: Option<String>,
    new_oid: Option<String>,
    old_regular: bool,
    new_regular: bool,
}

struct TrackedStat {
    path: String,
    additions: u64,
    deletions: u64,
}

struct TrackedChanges {
    files: Vec<ChangedFile>,
    records: Vec<PathRecord>,
    patch: String,
    stats: Vec<TrackedStat>,
    omitted_stats: HashSet<String>,
}

#[derive(Default)]
struct PatchChange {
    spans: Vec<LineSpan>,
    additions: u64,
    deletions: u64,
    start: usize,
    end: usize,
}

fn validate_revision(revision: &str) -> Result<(), String> {
    if revision.trim().is_empty()
        || revision.len() > 256
        || revision.trim_start().starts_with('-')
        || revision.chars().any(char::is_control)
    {
        Err("invalid Git revision".into())
    } else {
        Ok(())
    }
}

fn parse_tracked_changes(output: &[u8], cancelled: &AtomicBool) -> Result<TrackedChanges, String> {
    if output.is_empty() {
        return Ok(TrackedChanges {
            files: Vec::new(),
            records: Vec::new(),
            patch: String::new(),
            stats: Vec::new(),
            omitted_stats: HashSet::new(),
        });
    }
    let boundary = output
        .windows(2)
        .position(|bytes| bytes == b"\0\0")
        .ok_or_else(|| "Git returned malformed diff metadata".to_owned())?;
    let raw = &output[..=boundary];
    let patch = &output[boundary + 2..];
    let raw = parse_raw_changes(raw, cancelled)?;
    let patches = parse_patch_hunks(patch, raw.len(), cancelled)?;
    let mut files = Vec::new();
    let mut records = Vec::new();
    let mut stats = Vec::new();
    let mut omitted_stats = HashSet::new();
    let mut filtered_patch = Vec::new();

    for (index, (change, patch_change)) in raw.into_iter().zip(patches).enumerate() {
        check_progress(index, cancelled)?;
        let PatchChange {
            spans,
            additions,
            deletions,
            start,
            end,
        } = patch_change;
        let section_supported = match change.kind {
            RawKind::Added | RawKind::Modified => change
                .new
                .as_ref()
                .is_some_and(|path| change.new_regular && language_for_path(path).is_some()),
            RawKind::Deleted => change
                .old
                .as_ref()
                .is_some_and(|path| change.old_regular && language_for_path(path).is_some()),
            RawKind::Renamed => match (&change.old, &change.new) {
                (Some(old), Some(new)) => {
                    change.old_regular
                        && change.new_regular
                        && language_for_path(old).is_some()
                        && language_for_path(new).is_some()
                }
                _ => false,
            },
            RawKind::TypeChanged | RawKind::Unmerged => false,
        };
        let stat_path = match change.kind {
            RawKind::Added | RawKind::Modified => change
                .new
                .as_ref()
                .filter(|path| change.new_regular && language_for_path(path).is_some()),
            RawKind::Renamed => change
                .new
                .as_ref()
                .filter(|path| change.new_regular && language_for_path(path).is_some())
                .or_else(|| {
                    change
                        .old
                        .as_ref()
                        .filter(|path| change.old_regular && language_for_path(path).is_some())
                }),
            RawKind::Deleted => change
                .old
                .as_ref()
                .filter(|path| change.old_regular && language_for_path(path).is_some()),
            RawKind::TypeChanged | RawKind::Unmerged => None,
        };
        if let Some(path) = stat_path {
            if section_supported {
                stats.push(TrackedStat {
                    path: path.clone(),
                    additions,
                    deletions,
                });
            } else {
                omitted_stats.insert(path.clone());
            }
        }
        if section_supported {
            filtered_patch.extend_from_slice(&patch[start..end]);
        }
        match change.kind {
            RawKind::Added => {
                if let Some(path) = change
                    .new
                    .filter(|path| change.new_regular && language_for_path(path).is_some())
                {
                    files.push(ChangedFile {
                        path,
                        whole_file: true,
                        spans,
                        report_unmapped: true,
                    });
                }
            }
            RawKind::Modified => {
                if let Some(path) = change
                    .new
                    .filter(|path| change.new_regular && language_for_path(path).is_some())
                {
                    files.push(ChangedFile {
                        path,
                        whole_file: false,
                        spans,
                        report_unmapped: true,
                    });
                }
            }
            RawKind::Deleted => {
                if let Some(path) = change
                    .old
                    .filter(|path| change.old_regular && language_for_path(path).is_some())
                {
                    records.push(PathRecord::Deleted(path));
                }
            }
            RawKind::Renamed => match (change.old, change.new) {
                (Some(old), Some(new))
                    if change.old_regular
                        && change.new_regular
                        && language_for_path(&old).is_some()
                        && language_for_path(&new).is_some() =>
                {
                    files.push(ChangedFile {
                        path: new.clone(),
                        whole_file: true,
                        report_unmapped: !spans.is_empty(),
                        spans,
                    });
                    records.push(PathRecord::Renamed(old, new));
                }
                (Some(old), new) if change.old_regular && language_for_path(&old).is_some() => {
                    records.push(PathRecord::Deleted(old));
                    if let Some(new) =
                        new.filter(|path| change.new_regular && language_for_path(path).is_some())
                    {
                        files.push(ChangedFile {
                            path: new,
                            whole_file: true,
                            spans,
                            report_unmapped: true,
                        });
                    }
                }
                (_, Some(new)) if change.new_regular && language_for_path(&new).is_some() => {
                    files.push(ChangedFile {
                        path: new,
                        whole_file: true,
                        spans,
                        report_unmapped: true,
                    });
                }
                _ => {}
            },
            RawKind::TypeChanged | RawKind::Unmerged => {}
        }
    }
    Ok(TrackedChanges {
        files,
        records,
        patch: String::from_utf8_lossy(&filtered_patch).into_owned(),
        stats,
        omitted_stats,
    })
}

fn capture_tracked_artifacts(
    root: &Path,
    output: &[u8],
    source: ArtifactSource,
    cancelled: &AtomicBool,
) -> Result<TrackedArtifactSnapshot, String> {
    let mut hash = blake3::Hasher::new();
    hash.update(&(output.len() as u64).to_le_bytes());
    hash.update(output);
    if output.is_empty() {
        return Ok(TrackedArtifactSnapshot {
            review: ArtifactReview::default(),
            stats: Vec::new(),
            renames: Vec::new(),
        });
    }
    let boundary = output
        .windows(2)
        .position(|bytes| bytes == b"\0\0")
        .ok_or_else(|| "Git returned malformed artifact diff metadata".to_owned())?;
    let raw = parse_raw_changes(&output[..=boundary], cancelled)?;
    let patch = &output[boundary + 2..];
    let patches = parse_patch_hunks(patch, raw.len(), cancelled)?;
    let mut files = Vec::new();
    let mut analysis = Vec::new();
    let mut filtered_patch = String::new();
    let mut stats = Vec::new();
    let mut renames = Vec::new();

    for (index, (change, patch_change)) in raw.into_iter().zip(patches).enumerate() {
        check_progress(index, cancelled)?;
        if let Some(new_oid) = &change.new_oid {
            hash.update(new_oid.as_bytes());
        }
        let old_path = change.old.as_deref();
        let new_path = change.new.as_deref();
        let old_analyzer = if change.kind == RawKind::Modified {
            new_path
        } else {
            old_path
        }
        .map_or(AnalyzerKind::Generic, analyzer_kind);
        let new_analyzer = new_path.map_or(AnalyzerKind::Generic, analyzer_kind);
        let path = match change.kind {
            RawKind::Added | RawKind::Modified | RawKind::TypeChanged | RawKind::Unmerged => {
                change.new.as_deref()
            }
            RawKind::Deleted => change.old.as_deref(),
            RawKind::Renamed => match (change.old.as_deref(), change.new.as_deref()) {
                (Some(old), Some(new)) => {
                    renames.push((old.to_owned(), new.to_owned()));
                    Some(new)
                }
                _ => None,
            },
        };
        let Some(path) = path else {
            continue;
        };
        let section = &patch[patch_change.start..patch_change.end];
        let binary = section.contains(&0) || binary_patch_marker(section).is_some();
        let (diff_complete, mut omission) = if binary {
            if let Some(metadata) = binary_patch_metadata(section) {
                if filtered_patch.len().saturating_add(metadata.len()) > STDOUT_LIMIT {
                    return Err("Git output exceeded its limit".into());
                }
                filtered_patch.push_str(metadata);
            }
            (false, Some(ArtifactOmission::Binary))
        } else if let Ok(section) = std::str::from_utf8(section) {
            if filtered_patch.len().saturating_add(section.len()) > STDOUT_LIMIT {
                return Err("Git output exceeded its limit".into());
            }
            filtered_patch.push_str(section);
            stats.push(TrackedStat {
                path: path.to_owned(),
                additions: patch_change.additions,
                deletions: patch_change.deletions,
            });
            (true, None)
        } else {
            (false, Some(ArtifactOmission::InvalidUtf8))
        };
        let analyzer = if change.kind == RawKind::Deleted {
            old_analyzer
        } else {
            new_analyzer
        };
        let mut analysis_complete = false;
        if omission.is_none() {
            omission = match change.kind {
                RawKind::TypeChanged => Some(ArtifactOmission::TypeChanged),
                RawKind::Unmerged => Some(ArtifactOmission::Unmerged),
                _ if (matches!(
                    change.kind,
                    RawKind::Modified | RawKind::Deleted | RawKind::Renamed
                ) && !change.old_regular)
                    || (matches!(
                        change.kind,
                        RawKind::Added | RawKind::Modified | RawKind::Renamed
                    ) && !change.new_regular) =>
                {
                    Some(ArtifactOmission::NonRegular)
                }
                _ => None,
            };
        }
        if omission.is_none() {
            let mut old_text = None;
            let mut new_text = None;
            let old_required = old_analyzer != AnalyzerKind::Generic
                && matches!(
                    change.kind,
                    RawKind::Modified | RawKind::Deleted | RawKind::Renamed
                );
            let new_required = new_analyzer != AnalyzerKind::Generic
                && matches!(
                    change.kind,
                    RawKind::Added | RawKind::Modified | RawKind::Renamed
                );
            let mut old_omission = None;
            let mut new_omission = None;
            if old_required {
                let old = old_semantic_text(root, change.old_oid.as_deref(), cancelled)?;
                hash_semantic_input(&mut hash, b"old", &old);
                match old {
                    Ok(text) => old_text = Some(text),
                    Err(reason) => old_omission = Some(reason),
                }
            }
            if new_required {
                let new = match source {
                    ArtifactSource::GitBlob => {
                        old_semantic_text(root, change.new_oid.as_deref(), cancelled)?
                    }
                    ArtifactSource::Worktree => current_semantic_text(root, path, cancelled)?,
                };
                hash_semantic_input(&mut hash, b"new", &new);
                match new {
                    Ok(text) => new_text = Some(text),
                    Err(reason) => new_omission = Some(reason),
                }
            }
            let old_ready = !old_required || old_text.is_some();
            let new_ready = !new_required || new_text.is_some();
            if change.kind == RawKind::Renamed && old_analyzer != new_analyzer {
                if old_ready {
                    record_artifact_analysis(
                        old_path.expect("rename has old endpoint"),
                        old_text.as_deref(),
                        None,
                        &mut hash,
                        &mut analysis,
                    )?;
                }
                if new_ready {
                    record_artifact_analysis(
                        new_path.expect("rename has new endpoint"),
                        None,
                        new_text.as_deref(),
                        &mut hash,
                        &mut analysis,
                    )?;
                }
            } else if old_ready && new_ready {
                record_artifact_analysis(
                    path,
                    old_text.as_deref(),
                    new_text.as_deref(),
                    &mut hash,
                    &mut analysis,
                )?;
            }
            analysis_complete = old_ready && new_ready;
            omission = old_omission.or(new_omission);
        }
        if let Some(reason) = omission {
            hash.update(reason.as_str().as_bytes());
        }
        files.push(ArtifactFile {
            path: path.to_owned(),
            analyzer,
            diff_complete,
            analysis_complete,
            omission,
        });
    }
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    analysis.sort_unstable();
    let analysis = analysis.join("\n");
    if filtered_patch.len().saturating_add(analysis.len()) > STDOUT_LIMIT {
        return Err("Git output exceeded its limit".into());
    }
    Ok(TrackedArtifactSnapshot {
        review: ArtifactReview {
            files,
            analysis,
            patch: filtered_patch,
        },
        stats,
        renames,
    })
}

fn record_artifact_analysis(
    path: &str,
    old: Option<&str>,
    new: Option<&str>,
    hash: &mut blake3::Hasher,
    output: &mut Vec<String>,
) -> Result<(), String> {
    let result = analyze(path, old, new)?;
    hash.update(result.kind.as_str().as_bytes());
    hash.update(&(result.output.len() as u64).to_le_bytes());
    hash.update(result.output.as_bytes());
    if !result.output.is_empty() {
        output.push(result.output);
    }
    Ok(())
}

fn binary_patch_metadata(section: &[u8]) -> Option<&str> {
    let marker = binary_patch_marker(section)?;
    let end = section[marker..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(section.len(), |end| marker + end + 1);
    std::str::from_utf8(&section[..end]).ok()
}

fn binary_patch_marker(section: &[u8]) -> Option<usize> {
    let mut offset = 0;
    for line in section.split_inclusive(|byte| *byte == b'\n') {
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        if text.starts_with(b"Binary files ") || text == b"GIT binary patch" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn old_semantic_text(
    root: &Path,
    oid: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<Result<String, ArtifactOmission>, String> {
    let Some(oid) = oid else {
        return Ok(Err(ArtifactOmission::NonRegular));
    };
    match run_with_limit(
        root,
        &["cat-file", "blob", oid],
        SOURCE_LIMIT as usize + 1,
        cancelled,
    ) {
        Ok(content) => Ok(semantic_text(content)),
        Err(error) if error == "cannot read Git output: output limit exceeded" => {
            Ok(Err(ArtifactOmission::Oversized))
        }
        Err(error) => Err(error),
    }
}

fn current_semantic_text(
    root: &Path,
    path: &str,
    cancelled: &AtomicBool,
) -> Result<Result<String, ArtifactOmission>, String> {
    let Some(metadata) = safe_regular_metadata(root, path) else {
        return Ok(Err(ArtifactOmission::NonRegular));
    };
    if metadata.len() > SOURCE_LIMIT {
        return Ok(Err(ArtifactOmission::Oversized));
    }
    let Some(content) = read_regular_file(root, path, SOURCE_LIMIT, cancelled)? else {
        return Ok(Err(ArtifactOmission::NonRegular));
    };
    Ok(semantic_text(content))
}

fn semantic_text(content: Vec<u8>) -> Result<String, ArtifactOmission> {
    if content.len() as u64 > SOURCE_LIMIT {
        Err(ArtifactOmission::Oversized)
    } else if content.contains(&0) {
        Err(ArtifactOmission::Binary)
    } else {
        String::from_utf8(content).map_err(|_| ArtifactOmission::InvalidUtf8)
    }
}

fn hash_semantic_input(
    hash: &mut blake3::Hasher,
    side: &[u8],
    input: &Result<String, ArtifactOmission>,
) {
    hash.update(side);
    match input {
        Ok(text) => {
            hash.update(&(text.len() as u64).to_le_bytes());
            hash.update(text.as_bytes());
        }
        Err(reason) => {
            hash.update(reason.as_str().as_bytes());
        }
    };
}

fn parse_raw_changes(input: &[u8], cancelled: &AtomicBool) -> Result<Vec<RawChange>, String> {
    if !input.ends_with(&[0]) {
        return Err("Git returned malformed diff metadata".into());
    }
    let mut records = input.split(|byte| *byte == 0);
    let mut changes = Vec::new();
    while let Some(header) = records.next() {
        check_progress(changes.len(), cancelled)?;
        if header.is_empty() {
            if records.next().is_some() {
                return Err("Git returned malformed diff metadata".into());
            }
            break;
        }
        let header = parse_raw_header(header)?;
        let kind = header.kind;
        let first = records
            .next()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "Git returned malformed diff metadata".to_owned())?;
        let first = parse_change_path(first)?;
        let (old, new) = if matches!(kind, RawKind::Renamed) {
            let second = records
                .next()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| "Git returned malformed diff metadata".to_owned())?;
            (first, parse_change_path(second)?)
        } else if matches!(kind, RawKind::Deleted) {
            (first, None)
        } else {
            (None, first)
        };
        changes.push(RawChange {
            kind,
            old,
            new,
            old_oid: header.old_oid,
            new_oid: header.new_oid,
            old_regular: header.old_regular,
            new_regular: header.new_regular,
        });
    }
    Ok(changes)
}

fn parse_raw_header(header: &[u8]) -> Result<RawHeader, String> {
    let fields = header
        .strip_prefix(b":")
        .ok_or_else(|| "Git returned malformed diff metadata".to_owned())?
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() != 5
        || !fields[..2]
            .iter()
            .all(|mode| mode.len() == 6 && mode.iter().all(|byte| matches!(byte, b'0'..=b'7')))
        || !fields[2..4].iter().all(|oid| valid_oid(oid))
    {
        return Err("Git returned malformed diff metadata".into());
    }
    let kind = match fields[4] {
        b"A" => RawKind::Added,
        b"M" => RawKind::Modified,
        b"D" => RawKind::Deleted,
        b"T" => RawKind::TypeChanged,
        b"U" => RawKind::Unmerged,
        score
            if score.first() == Some(&b'R')
                && score.len() > 1
                && score[1..].iter().all(u8::is_ascii_digit)
                && std::str::from_utf8(&score[1..])
                    .ok()
                    .and_then(|score| score.parse::<u8>().ok())
                    .is_some_and(|score| score <= 100) =>
        {
            RawKind::Renamed
        }
        _ => return Err("Git returned unsupported diff metadata".into()),
    };
    Ok(RawHeader {
        kind,
        old_oid: fields[2]
            .iter()
            .any(|byte| *byte != b'0')
            .then(|| String::from_utf8(fields[2].to_vec()).expect("validated ASCII object ID")),
        new_oid: fields[3]
            .iter()
            .any(|byte| *byte != b'0')
            .then(|| String::from_utf8(fields[3].to_vec()).expect("validated ASCII object ID")),
        old_regular: matches!(fields[0], b"100644" | b"100755"),
        new_regular: matches!(fields[1], b"100644" | b"100755"),
    })
}

fn parse_change_inventory(
    input: &[u8],
    cancelled: &AtomicBool,
) -> Result<(Vec<ChangedPath>, usize), String> {
    if input.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let raw = parse_raw_changes(input, cancelled)?;
    let mut paths = Vec::with_capacity(raw.len());
    let mut skipped = 0;
    for (index, change) in raw.into_iter().enumerate() {
        check_progress(index, cancelled)?;
        let old_language = change
            .old_regular
            .then(|| change.old.as_deref().and_then(language_for_path))
            .flatten();
        let new_language = change
            .new_regular
            .then(|| change.new.as_deref().and_then(language_for_path))
            .flatten();
        let (status, old_path, old_language, path, language) = match change.kind {
            RawKind::Added | RawKind::Modified | RawKind::TypeChanged | RawKind::Unmerged => {
                let Some(path) = change.new else {
                    skipped += 1;
                    continue;
                };
                let language = if matches!(change.kind, RawKind::TypeChanged | RawKind::Unmerged) {
                    None
                } else {
                    new_language
                };
                (change.kind.status(), None, None, path, language)
            }
            RawKind::Deleted => {
                let Some(path) = change.old else {
                    skipped += 1;
                    continue;
                };
                (ChangeStatus::Deleted, None, None, path, old_language)
            }
            RawKind::Renamed => match (change.old, change.new) {
                (Some(old), Some(new)) => (
                    ChangeStatus::Renamed,
                    Some(old),
                    old_language,
                    new,
                    new_language,
                ),
                (None, Some(new)) => {
                    skipped += 1;
                    (ChangeStatus::Added, None, None, new, new_language)
                }
                (Some(old), None) => {
                    skipped += 1;
                    (ChangeStatus::Deleted, None, None, old, old_language)
                }
                (None, None) => {
                    skipped += 2;
                    continue;
                }
            },
        };
        paths.push(ChangedPath {
            status,
            old_path,
            old_language,
            language,
            path,
            additions: None,
            deletions: None,
            layers: Vec::new(),
        });
    }
    paths.sort_unstable_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.status.cmp(&right.status))
            .then_with(|| left.old_path.cmp(&right.old_path))
    });
    paths.dedup();
    Ok((paths, skipped))
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum SourceProjection {
    Current(String),
    Deleted(String),
    Renamed(String, String),
}

fn validate_source_projection(
    files: &[ChangedFile],
    records: &[PathRecord],
    paths: &[ChangedPath],
    dependency_mode: DependencyMode,
) -> Result<(), String> {
    let mut actual = files
        .iter()
        .map(|file| SourceProjection::Current(file.path.clone()))
        .collect::<Vec<_>>();
    actual.extend(records.iter().filter_map(|record| match record {
        PathRecord::Deleted(path) => Some(SourceProjection::Deleted(path.clone())),
        PathRecord::Renamed(old, new) => Some(SourceProjection::Renamed(old.clone(), new.clone())),
        PathRecord::Untracked(_) => None,
    }));

    let mut expected = Vec::new();
    for path in paths {
        if dependency_mode == DependencyMode::Boundary && changed_dependency_package(path).is_some()
        {
            continue;
        }
        match path.status {
            ChangeStatus::Added | ChangeStatus::Modified if path.language.is_some() => {
                expected.push(SourceProjection::Current(path.path.clone()));
            }
            ChangeStatus::Deleted if path.language.is_some() => {
                expected.push(SourceProjection::Deleted(path.path.clone()));
            }
            ChangeStatus::Renamed => {
                let old = path.old_path.as_deref();
                let old_supported = path.old_language.is_some();
                let new_supported = path.language.is_some();
                match (old, old_supported, new_supported) {
                    (Some(old), true, true) => {
                        expected.push(SourceProjection::Current(path.path.clone()));
                        expected.push(SourceProjection::Renamed(old.to_owned(), path.path.clone()));
                    }
                    (Some(old), true, false) => {
                        expected.push(SourceProjection::Deleted(old.to_owned()));
                    }
                    (_, false, true) => {
                        expected.push(SourceProjection::Current(path.path.clone()));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    actual.sort_unstable();
    actual.dedup();
    expected.sort_unstable();
    expected.dedup();
    if actual == expected {
        Ok(())
    } else {
        Err("Git change inventories disagree; retry".into())
    }
}

fn apply_captured_stats(
    paths: &mut [ChangedPath],
    stats: Vec<TrackedStat>,
    mut omitted: HashSet<String>,
    captured: &HashSet<String>,
    dependency_mode: DependencyMode,
) -> Result<(), String> {
    let mut by_path = HashMap::new();
    for stat in stats {
        if by_path
            .insert(stat.path, (stat.additions, stat.deletions))
            .is_some()
        {
            return Err("Git returned duplicate change statistics".into());
        }
    }
    for path in paths {
        if dependency_mode == DependencyMode::Boundary && changed_dependency_package(path).is_some()
        {
            continue;
        }
        let mut keys = Vec::with_capacity(2);
        match path.status {
            ChangeStatus::Added | ChangeStatus::Modified | ChangeStatus::Deleted
                if captured.contains(&path.path) =>
            {
                keys.push(path.path.as_str());
            }
            ChangeStatus::Renamed => {
                if captured.contains(&path.path) {
                    keys.push(path.path.as_str());
                }
                if let Some(old) = path
                    .old_path
                    .as_deref()
                    .filter(|old| captured.contains(*old))
                    && Some(old) != keys.first().copied()
                {
                    keys.push(old);
                }
            }
            _ => {}
        }
        let has_keys = !keys.is_empty();
        let mut totals = Some((0_u64, 0_u64));
        for key in keys {
            if let Some((additions, deletions)) = by_path.remove(key) {
                if let Some((total_additions, total_deletions)) = &mut totals {
                    *total_additions = total_additions
                        .checked_add(additions)
                        .ok_or_else(|| "Git patch additions exceed range".to_owned())?;
                    *total_deletions = total_deletions
                        .checked_add(deletions)
                        .ok_or_else(|| "Git patch deletions exceed range".to_owned())?;
                }
            } else if !omitted.remove(key) {
                return Err("Git change inventories disagree; retry".into());
            } else {
                totals = None;
            }
        }
        if let Some((additions, deletions)) = totals
            && has_keys
        {
            path.additions = Some(additions);
            path.deletions = Some(deletions);
        }
    }
    if by_path.is_empty() && omitted.is_empty() {
        Ok(())
    } else {
        Err("Git change inventories disagree; retry".into())
    }
}

fn coalesce_renames(
    paths: &mut Vec<ChangedPath>,
    rename_pairs: &[(String, String)],
) -> Result<(), String> {
    let mut consumed = HashSet::new();
    let mut renames = Vec::new();
    for (old, new) in rename_pairs {
        if paths.iter().any(|path| {
            path.status == ChangeStatus::Renamed
                && path.old_path.as_deref() == Some(old)
                && path.path == *new
        }) {
            continue;
        }
        let deleted = paths.iter().enumerate().find(|(index, path)| {
            !consumed.contains(index) && path.status == ChangeStatus::Deleted && path.path == *old
        });
        let added = paths.iter().enumerate().find(|(index, path)| {
            !consumed.contains(index) && path.status == ChangeStatus::Added && path.path == *new
        });
        let (Some((deleted_index, deleted)), Some((added_index, added))) = (deleted, added) else {
            return Err("Git change inventories disagree; retry".into());
        };
        consumed.insert(deleted_index);
        consumed.insert(added_index);
        renames.push(ChangedPath {
            status: ChangeStatus::Renamed,
            old_path: Some(old.clone()),
            old_language: deleted.language,
            path: new.clone(),
            language: added.language,
            additions: None,
            deletions: None,
            layers: Vec::new(),
        });
    }
    if !consumed.is_empty() {
        *paths = paths
            .drain(..)
            .enumerate()
            .filter_map(|(index, path)| (!consumed.contains(&index)).then_some(path))
            .chain(renames)
            .collect();
    }
    Ok(())
}

fn parse_patch_hunks(
    input: &[u8],
    file_count: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<PatchChange>, String> {
    if file_count == 0 || !input.starts_with(b"diff --git ") || !input.ends_with(b"\n") {
        return Err("Git returned malformed patch metadata".into());
    }
    let mut patches = (0..file_count)
        .map(|_| PatchChange::default())
        .collect::<Vec<_>>();
    let mut current: Option<usize> = None;
    let mut sections = 0;
    let mut in_hunk = false;
    let mut offset = 0;
    for (index, segment) in input.split_inclusive(|byte| *byte == b'\n').enumerate() {
        check_progress(index, cancelled)?;
        let line = segment.strip_suffix(b"\n").unwrap_or(segment);
        if line.starts_with(b"diff --git ") {
            if sections == file_count {
                return Err("Git diff changed while reading it; retry".into());
            }
            if let Some(previous) = current {
                patches[previous].end = offset;
            }
            current = Some(sections);
            patches[sections].start = offset;
            sections += 1;
            in_hunk = false;
        } else if line.starts_with(b"@@ ") {
            let current =
                current.ok_or_else(|| "Git returned a hunk without file metadata".to_owned())?;
            let span = parse_hunk(line)?;
            if patches[current]
                .spans
                .last()
                .is_some_and(|previous| previous.end >= span.start)
            {
                return Err("Git returned overlapping diff hunks".into());
            }
            patches[current].spans.push(span);
            in_hunk = true;
        } else if in_hunk {
            let current = current.expect("hunk requires a current patch");
            if line.starts_with(b"+") {
                patches[current].additions = patches[current]
                    .additions
                    .checked_add(1)
                    .ok_or_else(|| "Git patch additions exceed range".to_owned())?;
            } else if line.starts_with(b"-") {
                patches[current].deletions = patches[current]
                    .deletions
                    .checked_add(1)
                    .ok_or_else(|| "Git patch deletions exceed range".to_owned())?;
            }
        }
        offset += segment.len();
    }
    if sections != file_count {
        return Err("Git diff changed while reading it; retry".into());
    }
    if let Some(current) = current {
        patches[current].end = input.len();
    }
    Ok(patches)
}

fn parse_hunk(line: &[u8]) -> Result<LineSpan, String> {
    let body = line
        .strip_prefix(b"@@ -")
        .ok_or_else(|| "Git returned a malformed hunk".to_owned())?;
    let separator = body
        .windows(2)
        .position(|bytes| bytes == b" +")
        .ok_or_else(|| "Git returned a malformed hunk".to_owned())?;
    parse_hunk_side(&body[..separator])?;
    let new = &body[separator + 2..];
    let end = new
        .windows(3)
        .position(|bytes| bytes == b" @@")
        .ok_or_else(|| "Git returned a malformed hunk".to_owned())?;
    let (start, count) = parse_hunk_side(&new[..end])?;
    if count == 0 {
        let anchor = start
            .checked_mul(2)
            .and_then(|line| line.checked_add(1))
            .ok_or_else(|| "Git hunk exceeds the supported line range".to_owned())?;
        Ok(LineSpan {
            start: anchor,
            end: anchor,
        })
    } else {
        if start == 0 {
            return Err("Git returned a malformed hunk".into());
        }
        let last = start
            .checked_add(count - 1)
            .ok_or_else(|| "Git hunk exceeds the supported line range".to_owned())?;
        Ok(LineSpan {
            start: start
                .checked_mul(2)
                .ok_or_else(|| "Git hunk exceeds the supported line range".to_owned())?,
            end: last
                .checked_mul(2)
                .ok_or_else(|| "Git hunk exceeds the supported line range".to_owned())?,
        })
    }
}

fn parse_hunk_side(input: &[u8]) -> Result<(u64, u64), String> {
    let mut fields = input.split(|byte| *byte == b',');
    let start = parse_decimal(fields.next().unwrap_or_default())?;
    let count = fields.next().map(parse_decimal).transpose()?.unwrap_or(1);
    if fields.next().is_some() {
        return Err("Git returned a malformed hunk".into());
    }
    Ok((start, count))
}

fn parse_decimal(input: &[u8]) -> Result<u64, String> {
    if input.is_empty() || !input.iter().all(u8::is_ascii_digit) {
        return Err("Git returned a malformed hunk".into());
    }
    std::str::from_utf8(input)
        .expect("validated ASCII integer")
        .parse()
        .map_err(|_| "Git hunk exceeds the supported line range".to_owned())
}

fn merge_changes(
    tracked: TrackedChanges,
    artifacts: TrackedArtifactSnapshot,
    inventory: (Vec<ChangedPath>, usize),
    untracked: UntrackedSnapshot,
    dependency_mode: DependencyMode,
    cancelled: &AtomicBool,
) -> Result<WorktreeChanges, String> {
    let (mut paths, mut skipped_paths) = inventory;
    let TrackedChanges {
        mut files,
        mut records,
        mut patch,
        stats,
        omitted_stats,
    } = tracked;
    let TrackedArtifactSnapshot {
        review: mut artifact_review,
        stats: artifact_stats,
        renames: artifact_renames,
    } = artifacts;
    let captured = stats
        .iter()
        .map(|stat| stat.path.clone())
        .chain(omitted_stats.iter().cloned())
        .chain(artifact_stats.iter().map(|stat| stat.path.clone()))
        .collect::<HashSet<_>>();
    let mut all_stats = stats;
    all_stats.extend(artifact_stats);
    let mut rename_pairs = artifact_renames;
    rename_pairs.extend(records.iter().filter_map(|record| match record {
        PathRecord::Renamed(old, new) => Some((old.clone(), new.clone())),
        _ => None,
    }));
    rename_pairs.sort_unstable();
    rename_pairs.dedup();
    coalesce_renames(&mut paths, &rename_pairs)?;
    validate_source_projection(&files, &records, &paths, dependency_mode)?;
    apply_captured_stats(
        &mut paths,
        all_stats,
        omitted_stats,
        &captured,
        dependency_mode,
    )?;
    let UntrackedSnapshot {
        paths: untracked_paths,
        source_patch: untracked_source_patch,
        artifacts: mut untracked_artifacts,
        skipped_paths: untracked_skipped_paths,
    } = untracked;
    skipped_paths += untracked_skipped_paths;
    for (index, path) in untracked_paths.into_iter().enumerate() {
        check_progress(index, cancelled)?;
        if path.language.is_some()
            && (dependency_mode == DependencyMode::Full || dependency_package(&path.path).is_none())
        {
            records.push(PathRecord::Untracked(path.path.clone()));
            files.push(ChangedFile {
                path: path.path.clone(),
                whole_file: true,
                spans: Vec::new(),
                report_unmapped: true,
            });
        }
        paths.push(path);
    }
    let untracked_source_patch = String::from_utf8_lossy(&untracked_source_patch);
    if patch
        .len()
        .saturating_add(untracked_source_patch.len())
        .saturating_add(artifact_review.patch.len())
        .saturating_add(untracked_artifacts.patch.len())
        .saturating_add(artifact_review.analysis.len())
        .saturating_add(untracked_artifacts.analysis.len())
        .saturating_add(usize::from(
            !artifact_review.analysis.is_empty() && !untracked_artifacts.analysis.is_empty(),
        ))
        > STDOUT_LIMIT
    {
        return Err("Git output exceeded its limit".into());
    }
    patch.push_str(&untracked_source_patch);
    artifact_review.patch.push_str(&untracked_artifacts.patch);
    let mut analysis = artifact_review
        .analysis
        .lines()
        .chain(untracked_artifacts.analysis.lines())
        .collect::<Vec<_>>();
    analysis.sort_unstable();
    artifact_review.analysis = analysis.join("\n");
    artifact_review.files.append(&mut untracked_artifacts.files);
    artifact_review
        .files
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    let mut merged_artifacts: Vec<ArtifactFile> = Vec::with_capacity(artifact_review.files.len());
    for file in artifact_review.files {
        if let Some(previous) = merged_artifacts.last()
            && previous.path == file.path
        {
            if previous != &file {
                return Err("Git working tree changed while reading; retry".into());
            }
        } else {
            merged_artifacts.push(file);
        }
    }
    artifact_review.files = merged_artifacts;
    check_cancelled(cancelled)?;
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    let mut merged = Vec::<ChangedFile>::with_capacity(files.len());
    for (index, mut file) in files.into_iter().enumerate() {
        check_progress(index, cancelled)?;
        if let Some(previous) = merged.last_mut()
            && previous.path == file.path
        {
            previous.whole_file |= file.whole_file;
            previous.report_unmapped |= file.report_unmapped;
            previous.spans.append(&mut file.spans);
            previous
                .spans
                .sort_unstable_by_key(|span| (span.start, span.end));
        } else {
            merged.push(file);
        }
    }
    let current = merged
        .iter()
        .map(|file| file.path.as_str())
        .collect::<HashSet<_>>();
    records.retain(
        |record| !matches!(record, PathRecord::Deleted(path) if current.contains(path.as_str())),
    );
    check_cancelled(cancelled)?;
    records.sort_unstable();
    records.dedup();
    check_cancelled(cancelled)?;
    paths.sort_unstable_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.status.cmp(&right.status))
            .then_with(|| left.old_path.cmp(&right.old_path))
    });
    paths.dedup();
    finalize_artifact_omissions(&paths, &mut artifact_review, dependency_mode);
    Ok(WorktreeChanges {
        files: merged,
        records,
        paths,
        source_patch: patch,
        artifacts: artifact_review,
        skipped_paths,
    })
}

fn finalize_artifact_omissions(
    paths: &[ChangedPath],
    review: &mut ArtifactReview,
    dependency_mode: DependencyMode,
) {
    for path in paths {
        let omission = match path.status {
            ChangeStatus::TypeChanged => ArtifactOmission::TypeChanged,
            ChangeStatus::Unmerged => ArtifactOmission::Unmerged,
            _ if path.language.is_none() => ArtifactOmission::NonRegular,
            _ => continue,
        };
        if !matches!(parse_change_path(path.path.as_bytes()), Ok(Some(_)))
            || dependency_mode == DependencyMode::Boundary
                && dependency_package(&path.path).is_some()
            || review.files.iter().any(|file| file.path == path.path)
        {
            continue;
        }
        review.files.push(ArtifactFile {
            path: path.path.clone(),
            analyzer: analyzer_kind(&path.path),
            diff_complete: false,
            analysis_complete: false,
            omission: Some(omission),
        });
    }
    review
        .files
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
}

fn parse_change_path(input: &[u8]) -> Result<Option<String>, String> {
    let Ok(path) = std::str::from_utf8(input) else {
        return Ok(None);
    };
    if path.chars().any(char::is_control) {
        return Ok(None);
    }
    let relative = Path::new(path);
    if relative.is_absolute()
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err("Git returned an unsafe changed path".into());
    }
    Ok(Some(path.to_owned()))
}

fn parse_tree_inventory(output: &[u8]) -> Result<TargetInventory, String> {
    validate_nul_inventory(output)?;
    let mut inventory = TargetInventory {
        sources: BTreeMap::new(),
        cargo_manifests: BTreeSet::new(),
        unmerged_paths: BTreeSet::new(),
        skipped: 0,
    };
    for record in nul_records(output) {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "Git returned malformed tree metadata".to_owned())?;
        let fields = record[..tab]
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        if fields.len() != 3
            || fields[0].len() != 6
            || !fields[0].iter().all(u8::is_ascii_digit)
            || !matches!(fields[1], b"blob" | b"commit")
            || !valid_lower_oid(fields[2])
        {
            return Err("Git returned malformed tree metadata".into());
        }
        let Some((path, kind)) = parse_inventory_path(&record[tab + 1..], &mut inventory.skipped)?
        else {
            continue;
        };
        let regular = fields[1] == b"blob" && matches!(fields[0], b"100644" | b"100755");
        match kind {
            InventoryKind::Source(language) if regular => {
                let oid = std::str::from_utf8(fields[2])
                    .expect("validated lowercase object ID")
                    .to_owned();
                if inventory
                    .sources
                    .insert(path, (language, InventoryContent::GitBlob(oid)))
                    .is_some()
                {
                    return Err("Git returned duplicate tree paths".into());
                }
            }
            InventoryKind::Source(_) => inventory.skipped += 1,
            InventoryKind::CargoManifest if regular => {
                inventory.cargo_manifests.insert(path);
            }
            InventoryKind::CargoManifest => {}
        }
    }
    Ok(inventory)
}

fn parse_index_inventory(output: &[u8]) -> Result<TargetInventory, String> {
    validate_nul_inventory(output)?;
    let mut entries = BTreeMap::<String, (InventoryKind, Vec<(u8, Vec<u8>, String)>)>::new();
    let mut skipped = 0;
    for record in nul_records(output) {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "Git returned malformed index metadata".to_owned())?;
        let fields = record[..tab]
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        if fields.len() != 3
            || fields[0].len() != 6
            || !fields[0].iter().all(u8::is_ascii_digit)
            || !valid_lower_oid(fields[1])
            || !matches!(fields[2], b"0" | b"1" | b"2" | b"3")
        {
            return Err("Git returned malformed index metadata".into());
        }
        let Some((path, kind)) = parse_inventory_path(&record[tab + 1..], &mut skipped)? else {
            continue;
        };
        let stage = fields[2][0] - b'0';
        let mode = fields[0].to_vec();
        let oid = std::str::from_utf8(fields[1])
            .expect("validated lowercase object ID")
            .to_owned();
        entries
            .entry(path)
            .and_modify(|(_, values)| values.push((stage, mode.clone(), oid.clone())))
            .or_insert((kind, vec![(stage, mode, oid)]));
    }

    let mut inventory = TargetInventory {
        sources: BTreeMap::new(),
        cargo_manifests: BTreeSet::new(),
        unmerged_paths: BTreeSet::new(),
        skipped,
    };
    for (path, (kind, entries)) in entries {
        let Some((_, mode, oid)) = entries
            .as_slice()
            .first()
            .filter(|(stage, _, _)| entries.len() == 1 && *stage == 0)
        else {
            if matches!(kind, InventoryKind::Source(_)) {
                inventory.skipped += 1;
            }
            inventory.unmerged_paths.insert(path);
            continue;
        };
        let regular = matches!(mode.as_slice(), b"100644" | b"100755");
        match kind {
            InventoryKind::Source(language) if regular => {
                inventory
                    .sources
                    .insert(path, (language, InventoryContent::GitBlob(oid.clone())));
            }
            InventoryKind::Source(_) => inventory.skipped += 1,
            InventoryKind::CargoManifest if regular => {
                inventory.cargo_manifests.insert(path);
            }
            InventoryKind::CargoManifest => {}
        }
    }
    Ok(inventory)
}

fn parse_inventory_paths(output: &[u8]) -> Result<(BTreeSet<String>, usize), String> {
    validate_nul_inventory(output)?;
    let mut paths = BTreeSet::new();
    let mut skipped = 0;
    for record in nul_records(output) {
        if let Some((path, _)) = parse_inventory_path(record, &mut skipped)? {
            paths.insert(path);
        }
    }
    Ok((paths, skipped))
}

fn parse_inventory_path(
    raw_path: &[u8],
    skipped: &mut usize,
) -> Result<Option<(String, InventoryKind)>, String> {
    let Ok(path) = std::str::from_utf8(raw_path) else {
        if raw_path.ends_with(b".rs") || raw_path.ends_with(b".py") {
            *skipped += 1;
        }
        return Ok(None);
    };
    let Some(path) = parse_change_path(path.as_bytes())? else {
        if path.ends_with(".rs") || path.ends_with(".py") {
            *skipped += 1;
        }
        return Ok(None);
    };
    Ok(inventory_kind(&path).map(|kind| (path, kind)))
}

fn inventory_kind(path: &str) -> Option<InventoryKind> {
    language_for_path(path)
        .map(InventoryKind::Source)
        .or_else(|| {
            (path == "Cargo.toml" || path.ends_with("/Cargo.toml"))
                .then_some(InventoryKind::CargoManifest)
        })
}

fn validate_nul_inventory(output: &[u8]) -> Result<(), String> {
    if !output.is_empty() && !output.ends_with(&[0]) {
        Err("Git returned malformed file inventory".into())
    } else {
        Ok(())
    }
}

fn nul_records(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    input
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
}

fn language_for_path(path: &str) -> Option<Language> {
    let relative = Path::new(path);
    if path.chars().any(char::is_control)
        || relative.is_absolute()
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    if path.ends_with(".rs") {
        Some(Language::Rust)
    } else if path.ends_with(".py") {
        Some(Language::Python)
    } else if [".js", ".jsx", ".mjs", ".cjs"]
        .iter()
        .any(|extension| path.ends_with(extension))
    {
        Some(Language::JavaScript)
    } else if [".ts", ".tsx", ".mts", ".cts"]
        .iter()
        .any(|extension| path.ends_with(extension))
    {
        Some(Language::TypeScript)
    } else {
        None
    }
}

fn valid_oid(oid: &[u8]) -> bool {
    matches!(oid.len(), 40 | 64) && oid.iter().all(u8::is_ascii_hexdigit)
}

fn valid_lower_oid(oid: &[u8]) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
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
        check_cancelled(cancelled)?;
    }
    Ok(())
}

fn same_file_version(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.mode() == right.mode()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn validate_discovery_path(path: &Path) -> Result<(), OperationError> {
    if !path.is_absolute() {
        return Err(OperationError::new(
            ErrorCode::InvalidParameters,
            "requested root must be an absolute path",
        ));
    }
    let value = path.to_str().ok_or_else(|| {
        OperationError::new(
            ErrorCode::InvalidParameters,
            "requested root is not valid UTF-8",
        )
    })?;
    if value.chars().any(char::is_control) {
        return Err(OperationError::new(
            ErrorCode::InvalidParameters,
            "requested root contains control characters",
        ));
    }
    Ok(())
}

fn open_git_directory(path: &Path) -> Result<fs::Metadata, OperationError> {
    validate_discovery_path(path).map_err(|_| {
        OperationError::new(
            ErrorCode::GitMetadataInvalid,
            "Git path is not a valid absolute path",
        )
        .with_path("git_dir", path)
    })?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|_| {
            OperationError::new(ErrorCode::GitMetadataInvalid, "cannot open Git directory")
                .with_path("git_dir", path)
        })?;
    let metadata = file.metadata().map_err(|_| {
        OperationError::new(
            ErrorCode::GitMetadataInvalid,
            "cannot inspect Git directory",
        )
        .with_path("git_dir", path)
    })?;
    if !metadata.is_dir() {
        return Err(OperationError::new(
            ErrorCode::GitMetadataInvalid,
            "Git path is not a directory",
        )
        .with_path("git_dir", path));
    }
    Ok(metadata)
}

fn git_metadata_error(error: String) -> OperationError {
    if error.contains("cancelled") {
        OperationError::new(ErrorCode::JobCancelled, "Git operation cancelled")
    } else {
        OperationError::new(ErrorCode::GitMetadataInvalid, "Git metadata is invalid")
    }
}

fn parse_path(output: &[u8]) -> Result<PathBuf, String> {
    let value = std::str::from_utf8(output)
        .map_err(|_| "Git path is not valid UTF-8".to_owned())?
        .trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.chars().any(char::is_control) {
        Err("Git path is empty or contains control characters".into())
    } else {
        let path = PathBuf::from(value);
        path.is_absolute()
            .then_some(path)
            .ok_or_else(|| "Git path is not absolute".into())
    }
}

fn parse_value(output: &[u8]) -> Result<String, String> {
    let value = std::str::from_utf8(output)
        .map_err(|_| "Git value is not valid UTF-8".to_owned())?
        .trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.chars().any(char::is_control) {
        Err("Git value is empty or contains control characters".into())
    } else {
        Ok(value.to_owned())
    }
}

pub(crate) fn resolve_commit(
    root: &Path,
    revision: &str,
    label: &str,
    cancelled: &AtomicBool,
) -> Result<String, OperationError> {
    validate_revision(revision).map_err(|_| {
        OperationError::new(
            ErrorCode::InvalidParameters,
            format!("invalid {label} revision"),
        )
    })?;
    let expression = format!("{revision}^{{commit}}");
    let output =
        run(root, &["rev-parse", "--verify", &expression], cancelled).map_err(|error| {
            if error.contains("cancelled") {
                OperationError::new(ErrorCode::JobCancelled, "Git operation cancelled")
            } else {
                OperationError::new(
                    ErrorCode::RefNotFound,
                    format!("{label} revision does not name a commit"),
                )
            }
        })?;
    let oid = parse_value(&output).map_err(|_| {
        OperationError::new(
            ErrorCode::GitMetadataInvalid,
            format!("Git returned an invalid {label} object ID"),
        )
    })?;
    if !valid_lower_oid(oid.as_bytes()) {
        return Err(OperationError::new(
            ErrorCode::GitMetadataInvalid,
            format!("Git returned an invalid {label} object ID"),
        ));
    }
    Ok(oid)
}

fn run(cwd: &Path, args: &[&str], cancelled: &AtomicBool) -> Result<Vec<u8>, String> {
    run_git(
        cwd,
        args,
        false,
        false,
        Instant::now() + DEADLINE,
        STDOUT_LIMIT,
        None,
        cancelled,
    )
}

fn run_with_limit(
    cwd: &Path,
    args: &[&str],
    stdout_limit: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, String> {
    run_git(
        cwd,
        args,
        false,
        false,
        Instant::now() + DEADLINE,
        stdout_limit,
        None,
        cancelled,
    )
}

fn run_with_index(
    cwd: &Path,
    args: &[&str],
    index_file: &Path,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, String> {
    run_git(
        cwd,
        args,
        false,
        false,
        Instant::now() + DEADLINE,
        STDOUT_LIMIT,
        Some(index_file),
        cancelled,
    )
}

#[allow(clippy::too_many_arguments)] // One optional index preserves the shared safe runner.
fn run_git(
    cwd: &Path,
    args: &[&str],
    allow_diff_exit: bool,
    isolate_repository: bool,
    deadline: Instant,
    stdout_limit: usize,
    index_file: Option<&Path>,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("Git cancelled".into());
    }
    if Instant::now() >= deadline {
        return Err("Git timed out".into());
    }
    #[cfg(test)]
    if let Some(hook) = GIT_TEST_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        hook(cwd, args, index_file);
    }
    let mut command = git_command(cwd, isolate_repository, index_file);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start Git: {error}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (overflow_tx, overflow_rx) = mpsc::channel();
    let stdout_thread = thread::spawn({
        let overflow_tx = overflow_tx.clone();
        move || read_capped(stdout, stdout_limit, overflow_tx)
    });
    let stderr_thread = thread::spawn(move || read_capped(stderr, STDERR_LIMIT, overflow_tx));

    let status = loop {
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            break Err("Git cancelled".to_owned());
        }
        if overflow_rx.try_recv().is_ok() {
            let _ = child.kill();
            let _ = child.wait();
            break Err("Git output exceeded its limit".to_owned());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break Err("Git timed out".to_owned());
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("cannot wait for Git: {error}"));
            }
        }
    };

    let stdout = join(stdout_thread)?;
    let stderr = join(stderr_thread)?;
    let status = status?;
    if status.success() || allow_diff_exit && status.code() == Some(1) {
        Ok(stdout)
    } else {
        let detail = sanitize(&stderr);
        if detail.is_empty() {
            Err(format!("Git failed with {status}"))
        } else {
            Err(format!("Git failed: {detail}"))
        }
    }
}

fn git_command(cwd: &Path, isolate_repository: bool, index_file: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    command.args(["--no-pager", "-c", "core.fsmonitor=false"]);
    if isolate_repository {
        command.args(["-c", "core.attributesFile=/dev/null"]);
    }
    command
        .arg("-C")
        .arg(cwd)
        .env("LC_ALL", "C")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_ATTR_NOSYSTEM")
        .env_remove("GIT_LITERAL_PATHSPECS")
        .env_remove("GIT_GLOB_PATHSPECS")
        .env_remove("GIT_NOGLOB_PATHSPECS")
        .env_remove("GIT_ICASE_PATHSPECS");
    if let Some(index_file) = index_file {
        command.env("GIT_INDEX_FILE", index_file);
    } else {
        command.env_remove("GIT_INDEX_FILE");
    }
    if isolate_repository {
        command
            .env("GIT_DIR", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1");
    } else {
        command.env_remove("GIT_DIR");
    }
    command
}

fn read_capped(
    mut reader: impl Read,
    limit: usize,
    overflow: mpsc::Sender<()>,
) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(output);
        }
        if output.len() + read > limit {
            let _ = overflow.send(());
            return Err(io::Error::other("output limit exceeded"));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn join(thread: thread::JoinHandle<io::Result<Vec<u8>>>) -> Result<Vec<u8>, String> {
    thread
        .join()
        .map_err(|_| "Git output reader panicked".to_owned())?
        .map_err(|error| format!("cannot read Git output: {error}"))
}

fn sanitize(input: &[u8]) -> String {
    String::from_utf8_lossy(input)
        .chars()
        .flat_map(char::escape_default)
        .take(512)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_snapshot_for_test;
    use crate::workspace::SnapshotTarget;
    use std::sync::{Mutex, MutexGuard};

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    static GIT_TEST_HOOK_SERIAL: Mutex<()> = Mutex::new(());

    struct GitTestHookGuard {
        _serial: MutexGuard<'static, ()>,
    }

    impl Drop for GitTestHookGuard {
        fn drop(&mut self) {
            *GIT_TEST_HOOK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    fn git_test_hook(
        hook: impl FnMut(&Path, &[&str], Option<&Path>) + Send + 'static,
    ) -> GitTestHookGuard {
        let serial = GIT_TEST_HOOK_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut slot = GIT_TEST_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(slot.is_none());
        *slot = Some(Box::new(hook));
        drop(slot);
        GitTestHookGuard { _serial: serial }
    }

    #[test]
    fn captured_index_preserves_racy_clean_timestamp() {
        let root = initialized_repository("captured-index-time");
        fs::write(root.join("tracked.rs"), "pub fn before() {}\n").unwrap();
        test_git(&root, &["add", "--", "tracked.rs"]);
        test_git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let expected = fs::metadata(&repository.index_path)
            .unwrap()
            .modified()
            .unwrap();
        let capture_root = private_dir("captured-index-time-private");

        let captured = capture_index(
            &repository.index_path,
            &capture_root,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(
            fs::metadata(captured).unwrap().modified().unwrap(),
            expected
        );
    }

    #[test]
    fn capture_represents_commit_index_and_worktree_layers() {
        let root = initialized_repository("layered-capture");
        for (path, content) in [
            ("mixed.rs", "pub fn mixed_base() {}\n"),
            ("committed.rs", "pub fn committed_base() {}\n"),
            ("old.rs", "pub fn renamed() {}\n"),
            ("deleted.rs", "pub fn deleted() {}\n"),
            ("unstaged.rs", "pub fn unstaged_base() {}\n"),
            ("README.md", "# base\n"),
        ] {
            fs::write(root.join(path), content).unwrap();
        }
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "base"]);
        let base = git_output(&root, &["rev-parse", "HEAD"]);

        fs::write(root.join("mixed.rs"), "pub fn mixed_commit() {}\n").unwrap();
        fs::write(root.join("committed.rs"), "pub fn committed_change() {}\n").unwrap();
        fs::rename(root.join("old.rs"), root.join("renamed.rs")).unwrap();
        fs::write(root.join("README.md"), "# committed\n").unwrap();
        test_git(&root, &["add", "-A"]);
        test_git(&root, &["commit", "--quiet", "-m", "committed"]);
        let head = git_output(&root, &["rev-parse", "HEAD"]);

        fs::write(root.join("mixed.rs"), "pub fn mixed_staged() {}\n").unwrap();
        fs::write(root.join("staged.rs"), "pub fn staged() {}\n").unwrap();
        fs::remove_file(root.join("deleted.rs")).unwrap();
        fs::write(root.join("README.md"), "# staged\n").unwrap();
        test_git(
            &root,
            &[
                "add",
                "-A",
                "--",
                "mixed.rs",
                "staged.rs",
                "deleted.rs",
                "README.md",
            ],
        );

        fs::write(root.join("mixed.rs"), "pub fn mixed_unstaged() {}\n").unwrap();
        fs::write(root.join("unstaged.rs"), "pub fn unstaged_change() {}\n").unwrap();
        fs::write(root.join("README.md"), "# unstaged\n").unwrap();
        fs::write(root.join("untracked.rs"), "pub fn untracked() {}\n").unwrap();
        fs::write(root.join("untracked.tsv"), "id\tvalue\na\tone\n").unwrap();

        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let commit_root = private_dir("layered-commit");
        let index_root = private_dir("layered-index");
        let worktree_root = private_dir("layered-worktree");
        let commit = repository
            .capture_snapshot(
                &base,
                &head,
                &SnapshotTarget::Commit,
                DependencyMode::Boundary,
                &commit_root,
                &AtomicBool::new(false),
            )
            .unwrap();
        let index = repository
            .capture_snapshot(
                &base,
                &head,
                &SnapshotTarget::Index,
                DependencyMode::Boundary,
                &index_root,
                &AtomicBool::new(false),
            )
            .unwrap();
        let worktree = repository
            .capture_snapshot(
                &base,
                &head,
                &SnapshotTarget::Worktree {
                    include_untracked: true,
                },
                DependencyMode::Boundary,
                &worktree_root,
                &AtomicBool::new(false),
            )
            .unwrap();

        assert_eq!(commit.changed_files, 4);
        assert_eq!(index.changed_files, 6);
        assert_eq!(worktree.changed_files, 9);
        assert_eq!(commit.commits_base_to_head, 1);
        assert_eq!(index.commits_base_to_head, 1);
        assert_eq!(worktree.commits_base_to_head, 1);
        assert_eq!(
            status_manifest(&commit),
            [
                ("README.md", ChangeStatus::Modified, None),
                ("committed.rs", ChangeStatus::Modified, None),
                ("mixed.rs", ChangeStatus::Modified, None),
                ("renamed.rs", ChangeStatus::Renamed, Some("old.rs")),
            ]
        );
        assert_eq!(
            status_manifest(&index),
            [
                ("README.md", ChangeStatus::Modified, None),
                ("committed.rs", ChangeStatus::Modified, None),
                ("deleted.rs", ChangeStatus::Deleted, None),
                ("mixed.rs", ChangeStatus::Modified, None),
                ("renamed.rs", ChangeStatus::Renamed, Some("old.rs")),
                ("staged.rs", ChangeStatus::Added, None),
            ]
        );
        assert_eq!(
            status_manifest(&worktree),
            [
                ("README.md", ChangeStatus::Modified, None),
                ("committed.rs", ChangeStatus::Modified, None),
                ("deleted.rs", ChangeStatus::Deleted, None),
                ("mixed.rs", ChangeStatus::Modified, None),
                ("renamed.rs", ChangeStatus::Renamed, Some("old.rs")),
                ("staged.rs", ChangeStatus::Added, None),
                ("unstaged.rs", ChangeStatus::Modified, None),
                ("untracked.rs", ChangeStatus::Untracked, None),
                ("untracked.tsv", ChangeStatus::Untracked, None),
            ]
        );
        assert_eq!(change(&commit, "mixed.rs").layers, [ChangeLayer::Committed]);
        assert_eq!(
            change(&index, "mixed.rs").layers,
            [ChangeLayer::Committed, ChangeLayer::Staged]
        );
        assert_eq!(
            change(&worktree, "mixed.rs").layers,
            [
                ChangeLayer::Committed,
                ChangeLayer::Staged,
                ChangeLayer::Unstaged,
            ]
        );
        assert_eq!(change(&worktree, "staged.rs").layers, [ChangeLayer::Staged]);
        assert_eq!(
            change(&worktree, "unstaged.rs").layers,
            [ChangeLayer::Unstaged]
        );
        assert_eq!(
            change(&worktree, "untracked.rs").layers,
            [ChangeLayer::Untracked]
        );
        assert_eq!(
            change(&worktree, "deleted.rs").status,
            ChangeStatus::Deleted
        );
        let renamed = change(&worktree, "renamed.rs");
        assert_eq!(renamed.status, ChangeStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old.rs"));
        assert_eq!(renamed.layers, [ChangeLayer::Committed]);

        assert_eq!(
            source_text(&repository, &commit.sources, "mixed.rs"),
            "pub fn mixed_commit() {}\n"
        );
        assert_eq!(
            source_text(&repository, &index.sources, "mixed.rs"),
            "pub fn mixed_staged() {}\n"
        );
        assert_eq!(
            source_text(&repository, &worktree.sources, "mixed.rs"),
            "pub fn mixed_unstaged() {}\n"
        );
        assert!(
            !commit
                .sources
                .files
                .iter()
                .any(|file| file.path == "untracked.rs")
        );
        assert!(
            !index
                .sources
                .files
                .iter()
                .any(|file| file.path == "untracked.rs")
        );
        assert_eq!(
            source_text(&repository, &worktree.sources, "untracked.rs"),
            "pub fn untracked() {}\n"
        );

        assert!(
            commit
                .changes
                .source_patch
                .contains("+pub fn mixed_commit() {}")
        );
        assert!(!commit.changes.source_patch.contains("mixed_staged"));
        assert!(
            index
                .changes
                .source_patch
                .contains("+pub fn mixed_staged() {}")
        );
        assert!(!index.changes.source_patch.contains("mixed_unstaged"));
        assert!(
            worktree
                .changes
                .source_patch
                .contains("+pub fn mixed_unstaged() {}")
        );
        assert!(
            worktree
                .changes
                .source_patch
                .contains("+pub fn untracked() {}")
        );
        assert!(commit.changes.artifacts.patch.contains("+# committed"));
        assert!(!commit.changes.artifacts.patch.contains("+# staged"));
        assert!(index.changes.artifacts.patch.contains("+# staged"));
        assert!(!index.changes.artifacts.patch.contains("+# unstaged"));
        assert!(worktree.changes.artifacts.patch.contains("+# unstaged"));
        assert!(worktree.changes.artifacts.patch.contains("untracked.tsv"));
        for file in &commit.changes.artifacts.files {
            assert!(file.diff_complete && file.analysis_complete, "{file:?}");
        }
        for file in &index.changes.artifacts.files {
            assert!(file.diff_complete && file.analysis_complete, "{file:?}");
        }
        for file in &worktree.changes.artifacts.files {
            assert!(file.diff_complete && file.analysis_complete, "{file:?}");
        }
        let encoded = rmcp::serde_json::to_vec(&worktree.changes).unwrap();
        let decoded: WorktreeChanges = rmcp::serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, worktree.changes);

        for path in [commit_root, index_root, worktree_root, root] {
            fs::remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn capture_rejects_index_or_worktree_drift() {
        let root = initialized_repository("capture-drift");
        fs::write(root.join("tracked.rs"), "pub fn base() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "base"]);
        let base = git_output(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join("staged.rs"), "pub fn first() {}\n").unwrap();
        test_git(&root, &["add", "--", "staged.rs"]);
        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();

        let index_root = private_dir("index-drift");
        let hook_root = root.clone();
        let mut mutated = false;
        let hook = git_test_hook(move |cwd, args, index_file| {
            if !mutated
                && cwd == hook_root
                && index_file.is_some()
                && args == ["ls-files", "--stage", "-z"]
            {
                fs::write(hook_root.join("staged.rs"), "pub fn second() {}\n").unwrap();
                test_git(&hook_root, &["add", "--", "staged.rs"]);
                mutated = true;
            }
        });
        let error = match repository.capture_snapshot(
            &base,
            &base,
            &SnapshotTarget::Index,
            DependencyMode::Boundary,
            &index_root,
            &AtomicBool::new(false),
        ) {
            Err(error) => error,
            Ok(_) => panic!("index drift was accepted"),
        };
        drop(hook);
        assert_eq!(error.code, ErrorCode::CaptureChanged);
        assert!(!index_root.exists());

        fs::write(root.join("tracked.rs"), vec![b'a'; SOURCE_LIMIT as usize]).unwrap();
        let worktree_root = private_dir("worktree-drift");
        let hook_root = root.clone();
        let mut mutated = false;
        let hook = git_test_hook(move |cwd, args, index_file| {
            if !mutated && cwd == hook_root && index_file.is_some() && args.first() == Some(&"diff")
            {
                fs::write(
                    hook_root.join("tracked.rs"),
                    vec![b'b'; SOURCE_LIMIT as usize],
                )
                .unwrap();
                mutated = true;
            }
        });
        let error = match repository.capture_snapshot(
            &base,
            &base,
            &SnapshotTarget::Worktree {
                include_untracked: false,
            },
            DependencyMode::Boundary,
            &worktree_root,
            &AtomicBool::new(false),
        ) {
            Err(error) => error,
            Ok(_) => panic!("worktree drift was accepted"),
        };
        drop(hook);
        assert_eq!(error.code, ErrorCode::CaptureChanged);
        assert!(!worktree_root.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capture_rejects_index_flag_only_drift() {
        let root = initialized_repository("capture-flag-drift");
        fs::write(root.join("tracked.rs"), "pub fn base() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "base"]);
        let base = git_output(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join("tracked.rs"), "pub fn dirty() {}\n").unwrap();
        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let capture_root = private_dir("capture-flag-drift-output");

        let hook_root = root.clone();
        let mut mutated = false;
        let hook = git_test_hook(move |cwd, args, index_file| {
            if !mutated
                && cwd == hook_root
                && index_file.is_some()
                && args == ["ls-files", "--stage", "-z"]
            {
                test_git(
                    &hook_root,
                    &["update-index", "--skip-worktree", "tracked.rs"],
                );
                mutated = true;
            }
        });
        let result = repository.capture_snapshot(
            &base,
            &base,
            &SnapshotTarget::Worktree {
                include_untracked: false,
            },
            DependencyMode::Boundary,
            &capture_root,
            &AtomicBool::new(false),
        );
        drop(hook);

        assert!(
            git_output(&root, &["ls-files", "--stage", "-v"]).starts_with("S 100644 "),
            "Git did not retain the flag-only mutation"
        );
        match result {
            Err(error) => assert_eq!(error.code, ErrorCode::CaptureChanged),
            Ok(_) => panic!("index flag-only drift was accepted"),
        }
        assert!(!capture_root.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_capture_removes_private_directory() {
        let root = initialized_repository("capture-cleanup");
        fs::write(root.join("tracked.rs"), "pub fn base() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "base"]);
        let base = git_output(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join("staged.rs"), "pub fn first() {}\n").unwrap();
        test_git(&root, &["add", "--", "staged.rs"]);
        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let capture_root = private_dir("capture-cleanup-output");

        let hook_root = root.clone();
        let mut mutated = false;
        let hook = git_test_hook(move |cwd, args, index_file| {
            if !mutated
                && cwd == hook_root
                && index_file.is_some()
                && args == ["ls-files", "--stage", "-z"]
            {
                fs::write(hook_root.join("staged.rs"), "pub fn second() {}\n").unwrap();
                test_git(&hook_root, &["add", "--", "staged.rs"]);
                mutated = true;
            }
        });
        let result = repository.capture_snapshot(
            &base,
            &base,
            &SnapshotTarget::Index,
            DependencyMode::Boundary,
            &capture_root,
            &AtomicBool::new(false),
        );
        drop(hook);

        match result {
            Err(error) => assert_eq!(error.code, ErrorCode::CaptureChanged),
            Ok(_) => panic!("index drift was accepted"),
        }
        assert_eq!(
            git_output(&root, &["show", ":staged.rs"]),
            "pub fn second() {}"
        );
        assert!(
            !capture_root.exists(),
            "failed capture retained its private directory"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dirty_digest_covers_omitted_and_untracked_paths() {
        let root = initialized_repository("dirty-digest");
        fs::write(root.join("safe.rs"), "pub fn base() {}\n").unwrap();
        fs::write(root.join("note.txt"), "base\n").unwrap();
        fs::write(root.join("large.md"), vec![b'a'; SOURCE_LIMIT as usize + 1]).unwrap();
        std::os::unix::fs::symlink("one", root.join("link.rs")).unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "base"]);
        let base = git_output(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join("safe.rs"), "pub fn first() {}\n").unwrap();
        fs::write(root.join("note.txt"), "first\n").unwrap();
        fs::write(root.join("large.md"), vec![b'b'; SOURCE_LIMIT as usize + 1]).unwrap();
        fs::remove_file(root.join("link.rs")).unwrap();
        std::os::unix::fs::symlink("two", root.join("link.rs")).unwrap();
        fs::write(root.join("untracked.bin"), [0, 1, 2]).unwrap();
        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let capture = |label: &str| {
            let capture_root = private_dir(label);
            let capture = repository
                .capture_snapshot(
                    &base,
                    &base,
                    &SnapshotTarget::Worktree {
                        include_untracked: true,
                    },
                    DependencyMode::Boundary,
                    &capture_root,
                    &AtomicBool::new(false),
                )
                .unwrap();
            fs::remove_dir_all(capture_root).unwrap();
            capture
        };

        let first = capture("digest-first");
        assert_eq!(
            first.changes.artifacts.file("large.md").unwrap().omission,
            Some(ArtifactOmission::Oversized)
        );
        assert_eq!(
            first.changes.artifacts.file("link.rs").unwrap().omission,
            Some(ArtifactOmission::NonRegular)
        );
        assert_eq!(
            first
                .changes
                .artifacts
                .file("untracked.bin")
                .unwrap()
                .omission,
            Some(ArtifactOmission::Binary)
        );

        fs::write(root.join("safe.rs"), "pub fn second() {}\n").unwrap();
        let second = capture("digest-source");
        assert_ne!(first.dirty_digest, second.dirty_digest);
        fs::write(root.join("note.txt"), "second\n").unwrap();
        let third = capture("digest-artifact");
        assert_ne!(second.dirty_digest, third.dirty_digest);
        let mut large = vec![b'b'; SOURCE_LIMIT as usize + 1];
        *large.last_mut().unwrap() = b'c';
        fs::write(root.join("large.md"), large).unwrap();
        let fourth = capture("digest-oversized");
        assert_ne!(third.dirty_digest, fourth.dirty_digest);
        fs::remove_file(root.join("link.rs")).unwrap();
        fs::write(root.join("link.rs"), "regular now\n").unwrap();
        let fifth = capture("digest-type");
        assert_ne!(fourth.dirty_digest, fifth.dirty_digest);
        fs::write(root.join("untracked.bin"), [0, 1, 3]).unwrap();
        let sixth = capture("digest-untracked");
        assert_ne!(fifth.dirty_digest, sixth.dirty_digest);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_targets_have_exact_target_specific_reasons() {
        let root = initialized_repository("empty-targets");
        fs::write(root.join("lib.rs"), "pub fn unchanged() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "base"]);
        let base = git_output(&root, &["rev-parse", "HEAD"]);
        test_git(
            &root,
            &["commit", "--quiet", "--allow-empty", "-m", "empty"],
        );
        let head = git_output(&root, &["rev-parse", "HEAD"]);
        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();

        let cases = [
            (
                "empty-identical-oids",
                base.as_str(),
                base.as_str(),
                SnapshotTarget::Commit,
                NoChangeReason::IdenticalCommitOids,
                0,
            ),
            (
                "empty-identical-trees",
                base.as_str(),
                head.as_str(),
                SnapshotTarget::Commit,
                NoChangeReason::IdenticalTrees,
                1,
            ),
            (
                "empty-index",
                head.as_str(),
                head.as_str(),
                SnapshotTarget::Index,
                NoChangeReason::EmptyIndexDelta,
                0,
            ),
            (
                "empty-worktree",
                head.as_str(),
                head.as_str(),
                SnapshotTarget::Worktree {
                    include_untracked: true,
                },
                NoChangeReason::EmptyWorktreeDelta,
                0,
            ),
        ];
        for (label, case_base, case_head, target, reason, commits) in cases {
            let capture_root = private_dir(label);
            let capture = repository
                .capture_snapshot(
                    case_base,
                    case_head,
                    &target,
                    DependencyMode::Boundary,
                    &capture_root,
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert!(capture.changes.is_empty(), "{label}");
            assert_eq!(capture.changed_files, 0, "{label}");
            assert_eq!(capture.no_change_reason, Some(reason), "{label}");
            assert_eq!(capture.commits_base_to_head, commits, "{label}");
            assert_eq!(capture.dirty_digest.len(), 64, "{label}");
            fs::remove_dir_all(capture_root).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn change<'a>(capture: &'a SnapshotCapture, path: &str) -> &'a ChangedPath {
        capture
            .changes
            .paths
            .iter()
            .find(|change| change.path == path)
            .unwrap_or_else(|| panic!("missing {path}: {:?}", capture.changes.paths))
    }

    fn status_manifest(capture: &SnapshotCapture) -> Vec<(&str, ChangeStatus, Option<&str>)> {
        capture
            .changes
            .paths
            .iter()
            .map(|path| (path.path.as_str(), path.status, path.old_path.as_deref()))
            .collect()
    }

    fn source_text(repository: &Repository, snapshot: &SourceSnapshot, path: &str) -> String {
        let source = snapshot
            .files
            .iter()
            .find(|source| source.path == path)
            .unwrap_or_else(|| panic!("missing source {path}"));
        let bytes = match &source.content {
            SourceContent::GitBlob(oid) => run(
                &repository.root,
                &["cat-file", "blob", oid],
                &AtomicBool::new(false),
            )
            .unwrap(),
            SourceContent::Captured {
                relative_path,
                digest,
            } => read_captured_source(
                &snapshot.capture_root,
                relative_path,
                digest,
                &AtomicBool::new(false),
            )
            .unwrap(),
        };
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn unmerged_index_sources_are_omitted_and_counted_once() {
        let output = format!(
            "100644 {OID} 1\tconflict.rs\0\
             100644 {OID} 2\tconflict.rs\0\
             100644 {OID} 3\tconflict.rs\0"
        );

        let inventory = parse_index_inventory(output.as_bytes()).unwrap();

        assert!(inventory.sources.is_empty());
        assert_eq!(inventory.skipped, 1);
    }

    #[test]
    fn worktree_capture_does_not_readd_an_unmerged_source() {
        let root = initialized_repository("worktree-unmerged");
        fs::write(root.join("conflict.rs"), "fn base() {}\n").unwrap();
        test_git(&root, &["add", "--", "conflict.rs"]);
        test_git(&root, &["commit", "--quiet", "-m", "base"]);
        test_git(&root, &["switch", "--quiet", "-c", "side"]);
        fs::write(root.join("conflict.rs"), "fn side() {}\n").unwrap();
        test_git(&root, &["commit", "--quiet", "-am", "side"]);
        test_git(&root, &["switch", "--quiet", "main"]);
        fs::write(root.join("conflict.rs"), "fn main() {}\n").unwrap();
        test_git(&root, &["commit", "--quiet", "-am", "main"]);
        let merge = Command::new("git")
            .args(["merge", "--quiet", "side"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(!merge.status.success());

        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let capture_root = private_dir("worktree-unmerged-output");
        let snapshot = repository
            .capture_sources(
                &repository.head_oid,
                &SnapshotTarget::Worktree {
                    include_untracked: false,
                },
                &capture_root,
                &AtomicBool::new(false),
            )
            .unwrap();

        assert!(snapshot.files.is_empty());
        assert_eq!(snapshot.skipped, 1);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(capture_root).unwrap();
    }

    #[test]
    fn commit_inventory_ignores_non_source_gitlinks() {
        let output = format!(
            "160000 commit {OID}\tvendor\0\
             100644 blob {OID}\tsrc/lib.rs\0"
        );

        let inventory = parse_tree_inventory(output.as_bytes()).unwrap();

        assert_eq!(inventory.sources.len(), 1);
        assert!(inventory.sources.contains_key("src/lib.rs"));
        assert_eq!(inventory.skipped, 0);
    }

    #[test]
    fn commit_capture_reads_an_unchecked_out_branch() {
        let root = initialized_repository("commit-capture");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn main_only() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "main"]);
        test_git(&root, &["switch", "--quiet", "-c", "feature"]);
        fs::write(root.join("src/feature.rs"), "pub fn feature_only() {}\n").unwrap();
        test_git(&root, &["add", "--", "src/feature.rs"]);
        test_git(&root, &["commit", "--quiet", "-m", "feature"]);
        let feature_oid = git_output(&root, &["rev-parse", "HEAD"]);
        test_git(&root, &["switch", "--quiet", "main"]);

        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let capture_root = private_dir("commit-capture-output");
        let sources = repository
            .capture_sources(
                &feature_oid,
                &SnapshotTarget::Commit,
                &capture_root,
                &AtomicBool::new(false),
            )
            .unwrap();
        let graph =
            build_snapshot_for_test(&repository, &sources, &AtomicBool::new(false)).unwrap();

        assert!(sources.files.iter().any(|file| {
            file.path == "src/feature.rs" && matches!(file.content, SourceContent::GitBlob(_))
        }));
        assert!(graph.nodes.iter().any(|node| node.name == "feature_only"));
        assert!(
            !graph
                .nodes
                .iter()
                .any(|node| node.name == "main_only" && node.file_key == "src/feature.rs")
        );
        fs::remove_dir_all(capture_root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commit_capture_uses_original_blob_when_git_replace_is_configured() {
        let root = initialized_repository("commit-replacement");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn original_blob() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "original"]);
        let original_oid = git_output(&root, &["rev-parse", "HEAD:src/lib.rs"]);
        fs::write(
            root.join("replacement-source"),
            "pub fn replacement_blob() {}\n",
        )
        .unwrap();
        let replacement_oid = git_output(&root, &["hash-object", "-w", "replacement-source"]);
        fs::remove_file(root.join("replacement-source")).unwrap();
        test_git(&root, &["replace", &original_oid, &replacement_oid]);

        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();
        let capture_root = private_dir("commit-replacement-output");
        let sources = repository
            .capture_sources(
                &repository.head_oid,
                &SnapshotTarget::Commit,
                &capture_root,
                &AtomicBool::new(false),
            )
            .unwrap();
        let graph =
            build_snapshot_for_test(&repository, &sources, &AtomicBool::new(false)).unwrap();

        assert!(graph.nodes.iter().any(|node| node.name == "original_blob"));
        assert!(
            !graph
                .nodes
                .iter()
                .any(|node| node.name == "replacement_blob")
        );
        fs::remove_dir_all(capture_root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn index_capture_reads_the_worktree_specific_index() {
        let fixture_root = temp_root("linked-index");
        let main = fixture_root.join("main");
        let linked = fixture_root.join("linked");
        fs::create_dir_all(main.join("src")).unwrap();
        test_git(&main, &["init", "--quiet", "--initial-branch=main"]);
        test_git(&main, &["config", "user.name", "Graphr Test"]);
        test_git(&main, &["config", "user.email", "graphr@example.invalid"]);
        fs::write(main.join("src/lib.rs"), "pub fn baseline() {}\n").unwrap();
        test_git(&main, &["add", "--", "."]);
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
        fs::write(main.join("src/main_staged.rs"), "fn main_staged() {}\n").unwrap();
        test_git(&main, &["add", "--", "src/main_staged.rs"]);
        fs::write(
            linked.join("src/linked_staged.rs"),
            "fn linked_staged() {}\n",
        )
        .unwrap();
        test_git(&linked, &["add", "--", "src/linked_staged.rs"]);

        let repository = Repository::discover_cancelled(&linked, &AtomicBool::new(false)).unwrap();
        let capture_root = private_dir("linked-index-output");
        let sources = repository
            .capture_sources(
                &repository.head_oid,
                &SnapshotTarget::Index,
                &capture_root,
                &AtomicBool::new(false),
            )
            .unwrap();
        let paths = sources
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();

        assert!(paths.contains(&"src/linked_staged.rs"));
        assert!(!paths.contains(&"src/main_staged.rs"));
        test_git(
            &main,
            &["worktree", "remove", "--force", linked.to_str().unwrap()],
        );
        fs::remove_dir_all(capture_root).unwrap();
        fs::remove_dir_all(fixture_root).unwrap();
    }

    #[test]
    fn worktree_capture_freezes_dirty_and_optional_untracked_sources() {
        let root = initialized_repository("worktree-capture");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn clean() {}\n").unwrap();
        test_git(&root, &["add", "--", "."]);
        test_git(&root, &["commit", "--quiet", "-m", "baseline"]);
        fs::write(root.join("src/lib.rs"), "pub fn dirty_before() {}\n").unwrap();
        fs::write(root.join("src/untracked.rs"), "pub fn untracked() {}\n").unwrap();
        let repository = Repository::discover_cancelled(&root, &AtomicBool::new(false)).unwrap();

        let excluded_root = private_dir("worktree-untracked-excluded");
        let excluded = repository
            .capture_sources(
                &repository.head_oid,
                &SnapshotTarget::Worktree {
                    include_untracked: false,
                },
                &excluded_root,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(
            !excluded
                .files
                .iter()
                .any(|file| file.path == "src/untracked.rs")
        );

        let included_root = private_dir("worktree-untracked-included");
        let included = repository
            .capture_sources(
                &repository.head_oid,
                &SnapshotTarget::Worktree {
                    include_untracked: true,
                },
                &included_root,
                &AtomicBool::new(false),
            )
            .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn dirty_after() {}\n").unwrap();
        let graph =
            build_snapshot_for_test(&repository, &included, &AtomicBool::new(false)).unwrap();

        assert!(graph.nodes.iter().any(|node| node.name == "dirty_before"));
        assert!(graph.nodes.iter().any(|node| node.name == "untracked"));
        assert!(!graph.nodes.iter().any(|node| node.name == "dirty_after"));
        fs::remove_dir_all(excluded_root).unwrap();
        fs::remove_dir_all(included_root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_malformed_git_paths() {
        assert!(parse_path(b"").is_err());
        assert!(parse_path(b"/tmp/a\nb\n").is_err());
        assert!(parse_path(&[0xff]).is_err());
        assert_eq!(
            dependency_package(".cargo/vendor/sha2/src/lib.rs"),
            Some("sha2")
        );
        assert_eq!(dependency_package(".cargo/vendor/build.rs"), None);
    }

    #[test]
    fn diagnostics_are_terminal_safe() {
        let value = sanitize(b"bad\n\x1b[31m");
        assert!(!value.chars().any(char::is_control));
        assert!(value.len() <= 512);
    }

    #[test]
    fn parse_raw_header_retains_nonzero_oids() {
        let zero = "0".repeat(OID.len());
        let modified =
            parse_raw_header(format!(":100644 100644 {OID} {OID} M").as_bytes()).unwrap();
        assert_eq!(modified.old_oid.as_deref(), Some(OID));
        assert_eq!(modified.new_oid.as_deref(), Some(OID));

        let added = parse_raw_header(format!(":000000 100644 {zero} {OID} A").as_bytes()).unwrap();
        assert_eq!(added.old_oid, None);
        assert_eq!(added.new_oid.as_deref(), Some(OID));
    }

    #[test]
    fn retains_a_full_utf8_safe_patch() {
        let mut output = format!(
            ":100644 100644 {OID} {OID} M\0large.rs\0\0\
             diff --git a/large.rs b/large.rs\n\
             @@ -1 +1 @@\n-old\n+"
        )
        .into_bytes();
        output.push(0xff);
        output.extend(std::iter::repeat_n(b'x', 9 * 1024));
        output.push(b'\n');

        let tracked = parse_tracked_changes(&output, &AtomicBool::new(false)).unwrap();
        assert!(tracked.patch.len() > 8 * 1024);
        assert!(tracked.patch.contains('\u{fffd}'));
        assert!(tracked.patch.ends_with("xxx\n"));
        assert!(!tracked.patch.contains("[truncated]"));
        assert_eq!(
            (tracked.stats[0].additions, tracked.stats[0].deletions),
            (1, 1)
        );
    }

    #[test]
    fn tracked_nonregular_and_conflict_inventory_gets_omissions() {
        let zero = "0".repeat(OID.len());
        let inventory = format!(
            ":100644 120000 {OID} {OID} T\0typed.rs\0\
             :000000 000000 {zero} {zero} U\0conflict.rs\0\
             :160000 160000 {OID} {OID} M\0vendor/core.rs\0\
             :000000 120000 {zero} {OID} A\0link.rs\0"
        );
        let (paths, skipped) =
            parse_change_inventory(inventory.as_bytes(), &AtomicBool::new(false)).unwrap();

        assert_eq!(skipped, 0);
        assert_eq!(
            paths
                .iter()
                .map(|path| (path.path.as_str(), path.status, path.language))
                .collect::<Vec<_>>(),
            [
                ("conflict.rs", ChangeStatus::Unmerged, None),
                ("link.rs", ChangeStatus::Added, None),
                ("typed.rs", ChangeStatus::TypeChanged, None),
                ("vendor/core.rs", ChangeStatus::Modified, None),
            ]
        );

        let mut review = ArtifactReview::default();
        finalize_artifact_omissions(&paths, &mut review, DependencyMode::Boundary);
        for (path, omission) in [
            ("conflict.rs", ArtifactOmission::Unmerged),
            ("link.rs", ArtifactOmission::NonRegular),
            ("typed.rs", ArtifactOmission::TypeChanged),
            ("vendor/core.rs", ArtifactOmission::NonRegular),
        ] {
            assert_eq!(
                review.file(path).unwrap().omission,
                Some(omission),
                "{path}"
            );
        }
    }

    #[test]
    fn one_sided_unsafe_renames_keep_the_safe_endpoint() {
        let mut inventory = format!(":100644 100644 {OID} {OID} R100\0").into_bytes();
        inventory.extend_from_slice(b"bad\nname.txt\0safe.rs\0");
        inventory
            .extend_from_slice(format!(":100644 100644 {OID} {OID} R100\0safe.py\0").as_bytes());
        inventory.extend_from_slice(b"\xff.py\0");

        let (mut paths, skipped) =
            parse_change_inventory(&inventory, &AtomicBool::new(false)).unwrap();
        assert_eq!(skipped, 2);
        assert_eq!(
            paths
                .iter()
                .map(|path| (path.path.as_str(), path.status, path.language))
                .collect::<Vec<_>>(),
            [
                ("safe.py", ChangeStatus::Deleted, Some(Language::Python)),
                ("safe.rs", ChangeStatus::Added, Some(Language::Rust)),
            ]
        );
        let files = vec![ChangedFile {
            path: "safe.rs".into(),
            whole_file: true,
            spans: Vec::new(),
            report_unmapped: true,
        }];
        let records = vec![PathRecord::Deleted("safe.py".into())];
        validate_source_projection(&files, &records, &paths, DependencyMode::Boundary).unwrap();
        apply_captured_stats(
            &mut paths,
            vec![
                TrackedStat {
                    path: "safe.py".into(),
                    additions: 0,
                    deletions: 1,
                },
                TrackedStat {
                    path: "safe.rs".into(),
                    additions: 1,
                    deletions: 0,
                },
            ],
            HashSet::new(),
            &HashSet::from(["safe.py".into(), "safe.rs".into()]),
            DependencyMode::Boundary,
        )
        .unwrap();
        assert_eq!(
            paths
                .iter()
                .map(|path| (path.additions, path.deletions))
                .collect::<Vec<_>>(),
            [(Some(0), Some(1)), (Some(1), Some(0))]
        );
    }

    #[test]
    fn captured_stats_combine_source_artifact_rename_endpoints() {
        let mut paths = vec![ChangedPath {
            status: ChangeStatus::Renamed,
            old_path: Some("old.rs".into()),
            old_language: Some(Language::Rust),
            path: "new.txt".into(),
            language: None,
            additions: None,
            deletions: None,
            layers: Vec::new(),
        }];
        let result = apply_captured_stats(
            &mut paths,
            vec![
                TrackedStat {
                    path: "old.rs".into(),
                    additions: 0,
                    deletions: 2,
                },
                TrackedStat {
                    path: "new.txt".into(),
                    additions: 3,
                    deletions: 0,
                },
            ],
            HashSet::new(),
            &HashSet::from(["old.rs".into(), "new.txt".into()]),
            DependencyMode::Boundary,
        );

        assert!(result.is_ok());
        assert_eq!((paths[0].additions, paths[0].deletions), (Some(3), Some(2)));
    }

    #[test]
    fn finalizes_non_content_artifact_omissions() {
        let paths = vec![
            ChangedPath {
                status: ChangeStatus::TypeChanged,
                old_path: None,
                old_language: None,
                path: "typed.rs".into(),
                language: None,
                additions: None,
                deletions: None,
                layers: Vec::new(),
            },
            ChangedPath {
                status: ChangeStatus::Unmerged,
                old_path: None,
                old_language: None,
                path: "conflict.py".into(),
                language: None,
                additions: None,
                deletions: None,
                layers: Vec::new(),
            },
            ChangedPath {
                status: ChangeStatus::Modified,
                old_path: None,
                old_language: None,
                path: "link.rs".into(),
                language: None,
                additions: None,
                deletions: None,
                layers: Vec::new(),
            },
            ChangedPath {
                status: ChangeStatus::Modified,
                old_path: None,
                old_language: None,
                path: "covered.rs".into(),
                language: None,
                additions: None,
                deletions: None,
                layers: Vec::new(),
            },
            ChangedPath {
                status: ChangeStatus::Modified,
                old_path: None,
                old_language: None,
                path: "regular.rs".into(),
                language: Some(Language::Rust),
                additions: Some(1),
                deletions: Some(1),
                layers: Vec::new(),
            },
            ChangedPath {
                status: ChangeStatus::Modified,
                old_path: None,
                old_language: None,
                path: ".cargo/vendor/pkg/link.rs".into(),
                language: None,
                additions: None,
                deletions: None,
                layers: Vec::new(),
            },
        ];
        let mut review = ArtifactReview {
            files: vec![ArtifactFile {
                path: "covered.rs".into(),
                analyzer: AnalyzerKind::Generic,
                diff_complete: false,
                analysis_complete: false,
                omission: Some(ArtifactOmission::Binary),
            }],
            ..ArtifactReview::default()
        };
        finalize_artifact_omissions(&paths, &mut review, DependencyMode::Boundary);
        assert_eq!(
            review.file("typed.rs").unwrap().omission,
            Some(ArtifactOmission::TypeChanged)
        );
        assert_eq!(
            review.file("conflict.py").unwrap().omission,
            Some(ArtifactOmission::Unmerged)
        );
        assert_eq!(
            review.file("link.rs").unwrap().omission,
            Some(ArtifactOmission::NonRegular)
        );
        assert_eq!(
            review.file("covered.rs").unwrap().omission,
            Some(ArtifactOmission::Binary)
        );
        assert!(review.file("regular.rs").is_none());
        assert!(review.file(".cargo/vendor/pkg/link.rs").is_none());
        assert_eq!(review.files.len(), 4);
    }

    #[test]
    fn rejects_merged_payload_over_the_aggregate_limit() {
        let half = STDOUT_LIMIT / 2;
        let error = merge_changes(
            TrackedChanges {
                files: Vec::new(),
                records: Vec::new(),
                patch: String::new(),
                stats: Vec::new(),
                omitted_stats: HashSet::new(),
            },
            TrackedArtifactSnapshot {
                review: ArtifactReview {
                    analysis: "a".repeat(half),
                    ..ArtifactReview::default()
                },
                ..TrackedArtifactSnapshot::default()
            },
            (Vec::new(), 0),
            UntrackedSnapshot {
                paths: Vec::new(),
                source_patch: Vec::new(),
                artifacts: ArtifactReview {
                    analysis: "b".repeat(STDOUT_LIMIT - half),
                    ..ArtifactReview::default()
                },
                skipped_paths: 0,
            },
            DependencyMode::Boundary,
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert_eq!(error, "Git output exceeded its limit");
    }

    #[test]
    fn oversized_old_semantic_blob_is_classified_without_capture_failure() {
        let root = temp_root("oversized-old-blob");
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet"]);
        fs::write(root.join("large.md"), vec![b'a'; STDOUT_LIMIT + 1]).unwrap();
        let oid = String::from_utf8(
            run(
                &root,
                &["hash-object", "-w", "--", "large.md"],
                &AtomicBool::new(false),
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            old_semantic_text(&root, Some(oid.trim()), &AtomicBool::new(false)).unwrap(),
            Err(ArtifactOmission::Oversized)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tracked_patch_filters_every_unsupported_section() {
        let zero = "0".repeat(OID.len());
        let tracked = format!(
            ":100644 100644 {OID} {OID} M\0good.rs\0\
             :000000 120000 {zero} {OID} A\0link.rs\0\
             :160000 160000 {OID} {OID} M\0vendor/core.rs\0\
             :100644 120000 {OID} {OID} T\0typed.py\0\
             :000000 000000 {zero} {zero} U\0conflict.rs\0\
             :100644 100644 {OID} {OID} M\0notes.txt\0\
             :100644 100644 {OID} {OID} M\0bad\nname.rs\0\0\
             diff --git a/good.rs b/good.rs\n\
             @@ -1 +1 @@\n-old\n+SAFE_PATCH\n\
             diff --git a/link.rs b/link.rs\n\
             @@ -0,0 +1 @@\n+SYMLINK_SECRET\n\
             diff --git a/vendor/core.rs b/vendor/core.rs\n\
             @@ -1 +1 @@\n-old\n+GITLINK_SECRET\n\
             diff --git a/typed.py b/typed.py\n\
             @@ -1 +1 @@\n-old\n+TYPE_SECRET\n\
             diff --git a/conflict.rs b/conflict.rs\n\
             @@ -1 +1 @@\n-old\n+UNMERGED_SECRET\n\
             diff --git a/notes.txt b/notes.txt\n\
             @@ -1 +1 @@\n-old\n+UNSUPPORTED_SECRET\n\
             diff --git a/bad b/bad\n\
             @@ -1 +1 @@\n-old\n+UNSAFE_SECRET\n"
        );
        let tracked = parse_tracked_changes(tracked.as_bytes(), &AtomicBool::new(false)).unwrap();
        assert_eq!(tracked.files.len(), 1);
        assert_eq!(tracked.files[0].path, "good.rs");
        assert!(tracked.records.is_empty());
        assert_eq!(tracked.stats.len(), 1);
        assert!(tracked.omitted_stats.is_empty());
        assert!(tracked.patch.contains("SAFE_PATCH"));
        for secret in [
            "SYMLINK_SECRET",
            "GITLINK_SECRET",
            "TYPE_SECRET",
            "UNMERGED_SECRET",
            "UNSUPPORTED_SECRET",
            "UNSAFE_SECRET",
        ] {
            assert!(!tracked.patch.contains(secret), "{secret} leaked");
        }
    }

    #[test]
    fn unsafe_rename_patch_is_filtered_without_claiming_stats() {
        let tracked = format!(
            ":100644 100644 {OID} {OID} R100\0safe.rs\0bad\nname.rs\0\0\
             diff --git a/safe.rs b/bad\n\
             @@ -1 +1 @@\n-old\n+UNSAFE_RENAME_SECRET\n"
        );
        let zero = "0".repeat(OID.len());
        let mut inventory = format!(":100644 000000 {OID} {zero} D\0safe.rs\0").into_bytes();
        inventory
            .extend_from_slice(format!(":000000 100644 {zero} {OID} A\0bad\nname.rs\0").as_bytes());
        let changes = merge_changes(
            parse_tracked_changes(tracked.as_bytes(), &AtomicBool::new(false)).unwrap(),
            TrackedArtifactSnapshot::default(),
            parse_change_inventory(&inventory, &AtomicBool::new(false)).unwrap(),
            UntrackedSnapshot {
                paths: Vec::new(),
                source_patch: Vec::new(),
                artifacts: ArtifactReview::default(),
                skipped_paths: 0,
            },
            DependencyMode::Boundary,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(changes.source_patch.is_empty());
        assert_eq!(changes.skipped_paths, 1);
        assert_eq!(changes.paths.len(), 1);
        assert_eq!(changes.paths[0].path, "safe.rs");
        assert_eq!(changes.paths[0].status, ChangeStatus::Deleted);
        assert_eq!(changes.paths[0].additions, None);
        assert_eq!(changes.paths[0].deletions, None);
    }

    #[test]
    fn untracked_artifacts_have_no_fallback_sampling_caps() {
        const FILE_SIZE: usize = 8_193;
        const FILE_COUNT: usize = 257;

        let root = temp_root("untracked-artifact-caps");
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet"]);
        assert!(FILE_COUNT * FILE_SIZE > SOURCE_LIMIT as usize);
        let mut input = Vec::new();
        for index in 0..FILE_COUNT {
            let path = format!("file-{index:03}.txt");
            let mut content = vec![b'x'; FILE_SIZE];
            content[FILE_SIZE - 1] = b'\n';
            if index == FILE_COUNT - 1 {
                content[..9].copy_from_slice(b"FINAL-256");
            }
            fs::write(root.join(&path), content).unwrap();
            input.extend_from_slice(path.as_bytes());
            input.push(0);
        }

        let snapshot = capture_untracked(
            &root,
            &input,
            DependencyMode::Boundary,
            true,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(snapshot.paths.len(), FILE_COUNT);
        assert!(snapshot.source_patch.is_empty());
        assert_eq!(snapshot.paths.last().unwrap().additions, Some(1));
        assert_eq!(snapshot.paths.last().unwrap().deletions, Some(0));
        assert!(
            snapshot
                .artifacts
                .file("file-256.txt")
                .is_some_and(|file| file.diff_complete && file.analysis_complete)
        );
        assert!(
            snapshot
                .artifacts
                .patch
                .contains("diff --git a/file-256.txt b/file-256.txt")
        );
        assert!(snapshot.artifacts.patch.contains("+FINAL-256"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn untracked_artifact_per_file_limit_is_inclusive() {
        let root = temp_root("untracked-artifact-limit");
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet"]);
        fs::write(root.join("at-limit.txt"), vec![b'x'; SOURCE_LIMIT as usize]).unwrap();
        fs::write(
            root.join("over-limit.txt"),
            vec![b'x'; SOURCE_LIMIT as usize + 1],
        )
        .unwrap();

        let snapshot = capture_untracked(
            &root,
            b"at-limit.txt\0over-limit.txt\0",
            DependencyMode::Boundary,
            true,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(snapshot.paths[0].additions, Some(1));
        assert_eq!(snapshot.paths[0].deletions, Some(0));
        let at_limit = snapshot.artifacts.file("at-limit.txt").unwrap();
        assert!(at_limit.diff_complete);
        assert!(at_limit.analysis_complete);
        assert_eq!(at_limit.omission, None);
        assert!(snapshot.artifacts.patch.contains("at-limit.txt"));
        assert_eq!(
            snapshot.artifacts.file("over-limit.txt").unwrap().omission,
            Some(ArtifactOmission::Oversized)
        );
        assert_eq!(snapshot.paths[1].additions, None);
        assert_eq!(snapshot.paths[1].deletions, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inconsistent_supported_inventories_are_rejected() {
        let path = ChangedPath {
            status: ChangeStatus::Modified,
            old_path: None,
            old_language: None,
            path: "changed.rs".into(),
            language: Some(Language::Rust),
            additions: Some(1),
            deletions: Some(1),
            layers: Vec::new(),
        };
        assert!(validate_source_projection(&[], &[], &[path], DependencyMode::Boundary).is_err());

        let changes = WorktreeChanges {
            files: Vec::new(),
            records: Vec::new(),
            paths: Vec::new(),
            source_patch: "diff --git a/changed.rs b/changed.rs\n".into(),
            artifacts: Default::default(),
            skipped_paths: 0,
        };
        assert!(!changes.is_empty());
        assert_eq!(
            capture_error("Git change inventories disagree; retry".into()).code,
            ErrorCode::CaptureChanged
        );
    }

    #[test]
    fn parses_changed_files_and_rejects_malformed_streams() {
        let cancelled = AtomicBool::new(false);
        let zero = "0".repeat(OID.len());
        let tracked = format!(
            ":000000 100644 {zero} {OID} A\0added.rs\0\
             :100644 100644 {OID} {OID} M\0modified.rs\0\
             :100644 000000 {OID} {zero} D\0deleted.rs\0\
             :100644 100644 {OID} {OID} R100\0old.rs\0renamed.rs\0\0\
             diff --git a/added.rs b/added.rs\n\
             @@ -0,0 +1,2 @@\n+first\n+second\n\
             diff --git a/modified.rs b/modified.rs\n\
             @@ -2 +2 @@\n-old\n+new\n\
             @@ -9,2 +8,0 @@\n-gone\n-away\n\
             diff --git a/deleted.rs b/deleted.rs\n\
             @@ -1 +0,0 @@\n-deleted\n\
             diff --git a/old.rs b/renamed.rs\n\
             similarity index 100%\n\
             rename from old.rs\n\
             rename to renamed.rs\n"
        );
        let inventory = format!(
            ":000000 100644 {zero} {OID} A\0added.rs\0\
             :100644 100644 {OID} {OID} M\0modified.rs\0\
             :100644 000000 {OID} {zero} D\0deleted.rs\0\
             :100644 000000 {OID} {zero} D\0old.rs\0\
             :000000 100644 {zero} {OID} A\0renamed.rs\0"
        );
        let root = temp_root("changed-files");
        fs::create_dir_all(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("untracked.rs"), "fn untracked() {}\n").unwrap();
        let untracked = capture_untracked(
            &root,
            b"untracked.rs\0",
            DependencyMode::Boundary,
            true,
            &cancelled,
        )
        .unwrap();
        let expected_patch = format!(
            "{}{}",
            tracked.split_once("\0\0").unwrap().1,
            String::from_utf8_lossy(&untracked.source_patch)
        );
        let changes = merge_changes(
            parse_tracked_changes(tracked.as_bytes(), &cancelled).unwrap(),
            TrackedArtifactSnapshot::default(),
            parse_change_inventory(inventory.as_bytes(), &cancelled).unwrap(),
            untracked,
            DependencyMode::Boundary,
            &cancelled,
        )
        .unwrap();

        assert_eq!(
            changes,
            WorktreeChanges {
                files: vec![
                    ChangedFile {
                        path: "added.rs".into(),
                        whole_file: true,
                        spans: vec![LineSpan { start: 2, end: 4 }],
                        report_unmapped: true,
                    },
                    ChangedFile {
                        path: "modified.rs".into(),
                        whole_file: false,
                        spans: vec![
                            LineSpan { start: 4, end: 4 },
                            LineSpan { start: 17, end: 17 },
                        ],
                        report_unmapped: true,
                    },
                    ChangedFile {
                        path: "renamed.rs".into(),
                        whole_file: true,
                        spans: Vec::new(),
                        report_unmapped: false,
                    },
                    ChangedFile {
                        path: "untracked.rs".into(),
                        whole_file: true,
                        spans: Vec::new(),
                        report_unmapped: true,
                    },
                ],
                records: vec![
                    PathRecord::Deleted("deleted.rs".into()),
                    PathRecord::Renamed("old.rs".into(), "renamed.rs".into()),
                    PathRecord::Untracked("untracked.rs".into()),
                ],
                paths: vec![
                    ChangedPath {
                        status: ChangeStatus::Added,
                        old_path: None,
                        old_language: None,
                        path: "added.rs".into(),
                        language: Some(Language::Rust),
                        additions: Some(2),
                        deletions: Some(0),
                        layers: Vec::new(),
                    },
                    ChangedPath {
                        status: ChangeStatus::Deleted,
                        old_path: None,
                        old_language: None,
                        path: "deleted.rs".into(),
                        language: Some(Language::Rust),
                        additions: Some(0),
                        deletions: Some(1),
                        layers: Vec::new(),
                    },
                    ChangedPath {
                        status: ChangeStatus::Modified,
                        old_path: None,
                        old_language: None,
                        path: "modified.rs".into(),
                        language: Some(Language::Rust),
                        additions: Some(1),
                        deletions: Some(3),
                        layers: Vec::new(),
                    },
                    ChangedPath {
                        status: ChangeStatus::Renamed,
                        old_path: Some("old.rs".into()),
                        old_language: Some(Language::Rust),
                        path: "renamed.rs".into(),
                        language: Some(Language::Rust),
                        additions: Some(0),
                        deletions: Some(0),
                        layers: Vec::new(),
                    },
                    ChangedPath {
                        status: ChangeStatus::Untracked,
                        old_path: None,
                        old_language: None,
                        path: "untracked.rs".into(),
                        language: Some(Language::Rust),
                        additions: Some(1),
                        deletions: Some(0),
                        layers: vec![ChangeLayer::Untracked],
                    },
                ],
                source_patch: expected_patch,
                artifacts: Default::default(),
                skipped_paths: 0,
            }
        );
        assert!(changes.source_patch.contains("-old\n+new\n"));
        assert!(changes.source_patch.contains("rename from old.rs"));
        assert!(
            changes
                .source_patch
                .contains("diff --git a/untracked.rs b/untracked.rs")
        );
        assert!(changes.source_patch.contains("+fn untracked() {}"));
        fs::remove_dir_all(root).unwrap();
        assert!(
            parse_tracked_changes(&tracked.as_bytes()[..tracked.len() - 1], &cancelled).is_err()
        );
        assert!(
            capture_untracked(
                &temp_root("missing"),
                b"a.rs\0\0",
                DependencyMode::Boundary,
                false,
                &cancelled,
            )
            .is_err()
        );
        let skipped = merge_changes(
            TrackedChanges {
                files: Vec::new(),
                records: Vec::new(),
                patch: String::new(),
                stats: Vec::new(),
                omitted_stats: HashSet::new(),
            },
            TrackedArtifactSnapshot::default(),
            (Vec::new(), 0),
            capture_untracked(
                &temp_root("missing"),
                b"\xff.rs\0",
                DependencyMode::Boundary,
                false,
                &cancelled,
            )
            .unwrap(),
            DependencyMode::Boundary,
            &cancelled,
        )
        .unwrap();
        assert_eq!(skipped.skipped_paths, 1);
        assert!(!skipped.is_empty());
        assert!(parse_change_path(b"../not-rust.txt").is_err());
        assert_eq!(
            parse_change_path(b"not-source.txt").unwrap(),
            Some("not-source.txt".into())
        );
    }

    #[test]
    fn detects_a_source_version_change() {
        let path =
            std::env::temp_dir().join(format!("graphr-source-version-{}", std::process::id()));
        fs::write(&path, "a").unwrap();
        let before = fs::metadata(&path).unwrap();
        fs::write(&path, "changed").unwrap();
        let after = fs::metadata(&path).unwrap();
        assert!(!same_file_version(&before, &after));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn change_inventory_captures_untracked_artifacts_and_ignores_ignored_files() {
        let root = temp_root("changes");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests/fixtures")).unwrap();
        fs::write(root.join("src/domain.rs"), "pub fn before() {}\n").unwrap();
        fs::write(root.join("tests/fixtures/tracked.tsv"), "old\n").unwrap();
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["add", "--", "."]);
        test_git(
            &root,
            &[
                "-c",
                "user.name=Graphr Test",
                "-c",
                "user.email=graphr@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );

        fs::write(
            root.join("src/domain.rs"),
            "pub fn after() {}\npub fn added() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/fixtures/alias-registry.v1.tsv"),
            "one\ntwo\nthree",
        )
        .unwrap();
        fs::write(root.join("tests/fixtures/tracked.tsv"), "new\nextra\n").unwrap();
        fs::write(root.join("tests/fixtures/blob.bin"), [0, 1, 2]).unwrap();
        fs::write(
            root.join("tests/fixtures/large.data"),
            vec![b'x'; SOURCE_LIMIT as usize + 1],
        )
        .unwrap();
        fs::write(root.join("tests/fixtures/invalid.txt"), [0xff]).unwrap();
        std::os::unix::fs::symlink("alias-registry.v1.tsv", root.join("tests/fixtures/link.rs"))
            .unwrap();
        fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
        fs::write(root.join("bad\nname.txt"), "unsafe\n").unwrap();

        let repository = test_repository(&root);
        let changes = capture_current_review(&repository, DependencyMode::Boundary);

        assert_eq!(changes.skipped_paths, 1);
        assert!(!changes.paths.iter().any(|path| path.path == "ignored.txt"));
        assert_eq!(
            changes
                .paths
                .iter()
                .map(|path| {
                    (
                        path.path.as_str(),
                        path.status,
                        path.language,
                        path.additions,
                        path.deletions,
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (
                    "src/domain.rs",
                    ChangeStatus::Modified,
                    Some(Language::Rust),
                    Some(2),
                    Some(1),
                ),
                (
                    "tests/fixtures/alias-registry.v1.tsv",
                    ChangeStatus::Untracked,
                    None,
                    Some(3),
                    Some(0),
                ),
                (
                    "tests/fixtures/blob.bin",
                    ChangeStatus::Untracked,
                    None,
                    None,
                    None,
                ),
                (
                    "tests/fixtures/invalid.txt",
                    ChangeStatus::Untracked,
                    None,
                    None,
                    None,
                ),
                (
                    "tests/fixtures/large.data",
                    ChangeStatus::Untracked,
                    None,
                    None,
                    None,
                ),
                (
                    "tests/fixtures/link.rs",
                    ChangeStatus::Untracked,
                    None,
                    None,
                    None,
                ),
                (
                    "tests/fixtures/tracked.tsv",
                    ChangeStatus::Modified,
                    None,
                    Some(2),
                    Some(1),
                ),
            ]
        );
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].path, "src/domain.rs");
        assert!(changes.source_patch.contains("+pub fn added() {}"));
        assert!(!changes.source_patch.contains("tracked.tsv"));
        assert!(changes.artifacts.patch.contains("alias-registry.v1.tsv"));
        assert!(changes.artifacts.patch.contains("tracked.tsv"));
        assert!(
            changes
                .artifacts
                .analysis
                .contains("key_basis=first-column")
        );
        assert_eq!(
            changes
                .artifacts
                .file("tests/fixtures/blob.bin")
                .unwrap()
                .omission,
            Some(ArtifactOmission::Binary)
        );
        assert_eq!(
            changes
                .artifacts
                .file("tests/fixtures/large.data")
                .unwrap()
                .omission,
            Some(ArtifactOmission::Oversized)
        );
        assert_eq!(
            changes
                .artifacts
                .file("tests/fixtures/link.rs")
                .unwrap()
                .omission,
            Some(ArtifactOmission::NonRegular)
        );
        assert_eq!(
            changes
                .artifacts
                .file("tests/fixtures/invalid.txt")
                .unwrap()
                .omission,
            Some(ArtifactOmission::InvalidUtf8)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tracked_artifacts_are_separate_analyzed_and_classified() {
        let root = temp_root("tracked-artifacts");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn before() {}\n").unwrap();
        fs::write(root.join("README.md"), "See [REQ-1](docs/old.md).\n").unwrap();
        fs::write(root.join("data.tsv"), "id\tvalue\na\told\n").unwrap();
        fs::write(root.join("docs/old.txt"), "plain\n").unwrap();
        fs::write(root.join("docs/deleted.txt"), "deleted\n").unwrap();
        fs::write(root.join("blob.bin"), [0, 1, 2]).unwrap();
        fs::write(root.join("large.md"), vec![b'a'; SOURCE_LIMIT as usize + 1]).unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["add", "--", "."]);
        test_git(
            &root,
            &[
                "-c",
                "user.name=Graphr Test",
                "-c",
                "user.email=graphr@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );

        fs::write(root.join("src/lib.rs"), "pub fn after() {}\n").unwrap();
        fs::write(root.join("README.md"), "See [REQ-2](docs/new.md).\n").unwrap();
        fs::write(root.join("data.tsv"), "id\tvalue\na\tnew\n").unwrap();
        fs::rename(root.join("docs/old.txt"), root.join("docs/new.txt")).unwrap();
        fs::remove_file(root.join("docs/deleted.txt")).unwrap();
        fs::write(root.join("docs/added.txt"), "added\n").unwrap();
        test_git(&root, &["add", "--", "docs/added.txt", "docs/new.txt"]);
        fs::write(root.join("blob.bin"), [0, 3, 4]).unwrap();
        fs::write(root.join("large.md"), vec![b'b'; SOURCE_LIMIT as usize + 1]).unwrap();

        let repository = test_repository(&root);
        let changes = capture_current_review(&repository, DependencyMode::Boundary);

        assert!(changes.source_patch.contains("src/lib.rs"));
        assert!(!changes.source_patch.contains("README.md"));
        assert!(changes.artifacts.patch.contains("README.md"));
        assert!(changes.artifacts.patch.contains("data.tsv"));
        assert!(changes.artifacts.patch.contains("docs/new.txt"));
        assert!(changes.artifacts.patch.contains("docs/deleted.txt"));
        assert!(changes.artifacts.patch.contains("docs/added.txt"));
        assert!(!changes.artifacts.patch.contains("src/lib.rs"));
        assert!(changes.artifacts.analysis.contains("REQ-1"));
        assert!(changes.artifacts.analysis.contains("REQ-2"));
        assert_eq!(
            changes.artifacts.file("blob.bin").unwrap().omission,
            Some(ArtifactOmission::Binary)
        );
        let large = changes.artifacts.file("large.md").unwrap();
        assert!(large.diff_complete);
        assert!(!large.analysis_complete);
        assert_eq!(large.omission, Some(ArtifactOmission::Oversized));
        assert!(changes.paths.iter().any(|path| {
            path.status == ChangeStatus::Renamed
                && path.old_path.as_deref() == Some("docs/old.txt")
                && path.path == "docs/new.txt"
        }));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tracked_forced_text_nul_is_binary() {
        let root = temp_root("forced-text-nul");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".gitattributes"), "forced.dat diff\n").unwrap();
        fs::write(root.join("forced.dat"), b"old\0value\n").unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["add", "--", "."]);
        test_git(
            &root,
            &[
                "-c",
                "user.name=Graphr Test",
                "-c",
                "user.email=graphr@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );
        fs::write(root.join("forced.dat"), b"new\0value\n").unwrap();

        let changes = capture_current_review(&test_repository(&root), DependencyMode::Boundary);
        let file = changes.artifacts.file("forced.dat").unwrap();

        assert!(!file.diff_complete);
        assert!(!file.analysis_complete);
        assert_eq!(file.omission, Some(ArtifactOmission::Binary));
        assert!(!changes.artifacts.patch.as_bytes().contains(&0));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tracked_text_marker_phrases_remain_text() {
        let root = temp_root("text-binary-markers");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("notes.txt"), "before\n").unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["add", "--", "."]);
        test_git(
            &root,
            &[
                "-c",
                "user.name=Graphr Test",
                "-c",
                "user.email=graphr@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );
        fs::write(
            root.join("notes.txt"),
            "Binary files are ordinary text\nGIT binary patch is ordinary text\n",
        )
        .unwrap();

        let changes = capture_current_review(&test_repository(&root), DependencyMode::Boundary);
        let file = changes.artifacts.file("notes.txt").unwrap();

        assert!(file.diff_complete);
        assert!(file.analysis_complete);
        assert_eq!(file.omission, None);
        assert!(
            changes
                .artifacts
                .patch
                .contains("+Binary files are ordinary text")
        );
        assert!(
            changes
                .artifacts
                .patch
                .contains("+GIT binary patch is ordinary text")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tracked_source_extension_nonregular_paths_have_omissions() {
        let root = temp_root("tracked-source-nonregular");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("target-a"), "a\n").unwrap();
        fs::write(root.join("target-b"), "b\n").unwrap();
        std::os::unix::fs::symlink("target-a", root.join("link.rs")).unwrap();
        fs::write(root.join("typed.py"), "regular\n").unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["add", "--", "."]);
        test_git(
            &root,
            &[
                "-c",
                "user.name=Graphr Test",
                "-c",
                "user.email=graphr@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );
        fs::remove_file(root.join("link.rs")).unwrap();
        std::os::unix::fs::symlink("target-b", root.join("link.rs")).unwrap();
        fs::remove_file(root.join("typed.py")).unwrap();
        std::os::unix::fs::symlink("target-a", root.join("typed.py")).unwrap();
        let head =
            String::from_utf8(run(&root, &["rev-parse", "HEAD"], &AtomicBool::new(false)).unwrap())
                .unwrap();
        let cacheinfo = format!("160000,{},gitlink.rs", head.trim());
        test_git(&root, &["update-index", "--add", "--cacheinfo", &cacheinfo]);
        fs::create_dir(root.join("gitlink.rs")).unwrap();

        let changes = capture_current_review(&test_repository(&root), DependencyMode::Boundary);

        for (path, omission) in [
            ("link.rs", ArtifactOmission::NonRegular),
            ("typed.py", ArtifactOmission::TypeChanged),
            ("gitlink.rs", ArtifactOmission::NonRegular),
        ] {
            let file = changes
                .artifacts
                .file(path)
                .unwrap_or_else(|| panic!("missing {path}: {:?}", changes.paths));
            assert!(!file.diff_complete, "{path}");
            assert!(!file.analysis_complete, "{path}");
            assert_eq!(file.omission, Some(omission), "{path}");
        }
        assert!(changes.files.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn markdown_to_generic_rename_preserves_removed_semantics() {
        let root = temp_root("markdown-to-generic");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("old.md"), "See [REQ-1](docs/old.md).\n").unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["add", "--", "."]);
        test_git(
            &root,
            &[
                "-c",
                "user.name=Graphr Test",
                "-c",
                "user.email=graphr@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );
        fs::rename(root.join("old.md"), root.join("new.txt")).unwrap();
        test_git(&root, &["add", "-A"]);

        let changes = capture_current_review(&test_repository(&root), DependencyMode::Boundary);

        assert!(
            changes.artifacts.analysis.contains(
                "markdown path=\"old.md\" change=removed kind=requirement value=\"REQ-1\""
            )
        );
        assert_eq!(
            changes.artifacts.file("new.txt").unwrap().analyzer,
            AnalyzerKind::Generic
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generic_to_markdown_rename_preserves_added_semantics() {
        let root = temp_root("generic-to-markdown");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("old.txt"), "See [REQ-2](docs/new.md).\n").unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["add", "--", "."]);
        test_git(
            &root,
            &[
                "-c",
                "user.name=Graphr Test",
                "-c",
                "user.email=graphr@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );
        fs::rename(root.join("old.txt"), root.join("new.md")).unwrap();
        test_git(&root, &["add", "-A"]);

        let changes = capture_current_review(&test_repository(&root), DependencyMode::Boundary);

        assert!(
            changes
                .artifacts
                .analysis
                .contains("markdown path=\"new.md\" change=added kind=requirement value=\"REQ-2\"")
        );
        assert_eq!(
            changes.artifacts.file("new.md").unwrap().analyzer,
            AnalyzerKind::Markdown
        );
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

    fn capture_current_review(
        repository: &Repository,
        dependency_mode: DependencyMode,
    ) -> WorktreeChanges {
        let head = git_output(&repository.root, &["rev-parse", "HEAD"]);
        let capture_root = private_dir("worktree-review");
        let capture = repository
            .capture_snapshot(
                &head,
                &head,
                &SnapshotTarget::Worktree {
                    include_untracked: true,
                },
                dependency_mode,
                &capture_root,
                &AtomicBool::new(false),
            )
            .unwrap();
        let changes = capture.changes;
        fs::remove_dir_all(capture_root).unwrap();
        changes
    }

    fn initialized_repository(label: &str) -> PathBuf {
        let root = temp_root(label);
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--quiet", "--initial-branch=main"]);
        test_git(&root, &["config", "user.name", "Graphr Test"]);
        test_git(&root, &["config", "user.email", "graphr@example.invalid"]);
        root
    }

    fn private_dir(label: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;

        let root = temp_root(label);
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        fs::canonicalize(root).unwrap()
    }

    fn git_output(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn test_repository(root: &Path) -> Repository {
        let root = fs::canonicalize(root).unwrap();
        let git_dir = root.join(".git");
        Repository {
            root,
            common_git_dir: git_dir.clone(),
            common_git_dir_dev: fs::metadata(&git_dir).unwrap().dev(),
            common_git_dir_ino: fs::metadata(&git_dir).unwrap().ino(),
            index_path: git_dir.join("index"),
            git_dir,
            branch: None,
            head_oid: OID.into(),
            object_format: "sha1".into(),
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        fs::canonicalize(std::env::temp_dir())
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!(
                "graphr-git-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
    }
}
