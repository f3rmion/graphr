use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::git::{DependencyMode, Repository, resolve_commit};

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

    use crate::git::DependencyMode;

    use super::{AllowedRoots, ErrorCode, IndexRequest, SnapshotTarget, resolve_request};

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
