use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use crate::artifact::{AnalyzerKind, analyze, analyzer_kind};

const STDOUT_LIMIT: usize = 64 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);
const SOURCE_LIMIT: u64 = 2 * 1024 * 1024;

pub struct Repository {
    pub root: PathBuf,
    pub database: PathBuf,
}

pub struct Source {
    pub path: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Language {
    Rust,
    Python,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            _ => None,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct SourceFile {
    pub path: String,
    pub git_oid: Option<String>,
    pub language: Language,
}

pub struct SourceFiles {
    pub files: Vec<SourceFile>,
    pub skipped: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineSpan {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ChangedFile {
    pub path: String,
    pub whole_file: bool,
    pub spans: Vec<LineSpan>,
    pub report_unmapped: bool,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PathRecord {
    Deleted(String),
    Renamed(String, String),
    Untracked(String),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
    Unmerged,
    Untracked,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ChangedPath {
    pub status: ChangeStatus,
    pub old_path: Option<String>,
    pub old_language: Option<Language>,
    pub path: String,
    pub language: Option<Language>,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
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

#[derive(Debug, Eq, PartialEq)]
pub struct WorktreeChanges {
    pub files: Vec<ChangedFile>,
    pub records: Vec<PathRecord>,
    pub paths: Vec<ChangedPath>,
    pub source_patch: String,
    pub artifacts: ArtifactReview,
    pub skipped_paths: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Debug, Eq, PartialEq)]
pub struct ArtifactFile {
    pub path: String,
    pub analyzer: AnalyzerKind,
    pub diff_complete: bool,
    pub analysis_complete: bool,
    pub omission: Option<ArtifactOmission>,
}

#[derive(Debug, Default, Eq, PartialEq)]
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

struct WorktreeCapture {
    tracked: Vec<u8>,
    artifacts: TrackedArtifactSnapshot,
    inventory: Vec<u8>,
    untracked: UntrackedSnapshot,
}

#[derive(Default)]
struct TrackedArtifactSnapshot {
    review: ArtifactReview,
    stats: Vec<TrackedStat>,
    renames: Vec<(String, String)>,
    signature: [u8; 32],
}

struct UntrackedSnapshot {
    paths: Vec<ChangedPath>,
    source_patch: Vec<u8>,
    artifacts: ArtifactReview,
    skipped_paths: usize,
    signature: [u8; 32],
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
    pub fn discover_cancelled(path: &Path, cancelled: &AtomicBool) -> Result<Self, String> {
        validate_utf8(path, "project path")?;
        let path = fs::canonicalize(path)
            .map_err(|error| format!("cannot resolve project path: {error}"))?;
        if !path.is_dir() {
            return Err("project path is not a directory".into());
        }
        validate_utf8(&path, "project path")?;

        let root = parse_path(&run(
            &path,
            &["rev-parse", "--path-format=absolute", "--show-toplevel"],
            cancelled,
        )?)?;
        let root =
            fs::canonicalize(root).map_err(|error| format!("cannot resolve Git root: {error}"))?;
        if !path.starts_with(&root) {
            return Err("Git returned a root outside the project path".into());
        }

        let git_dir = parse_path(&run(
            &root,
            &["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
            cancelled,
        )?)?;
        let git_dir = fs::canonicalize(git_dir)
            .map_err(|error| format!("cannot resolve Git directory: {error}"))?;
        let database = parse_path(&run(
            &root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "graphr/index.db",
            ],
            cancelled,
        )?)?;
        if database != git_dir.join("graphr/index.db") {
            return Err("Git returned an unsafe database path".into());
        }
        validate_database_path(&git_dir, &database)?;

        Ok(Self { root, database })
    }

    pub fn source_files(&self, cancelled: &AtomicBool) -> Result<SourceFiles, String> {
        let output = run(
            &self.root,
            &[
                "ls-files",
                "--cached",
                "--modified",
                "--deleted",
                "--others",
                "--stage",
                "-v",
                "-z",
                "--exclude-standard",
                "--",
                "*.rs",
                "*.py",
            ],
            cancelled,
        )?;
        check_cancelled(cancelled)?;
        let mut inventory = parse_source_files(&output)?;
        let mut files = Vec::with_capacity(inventory.files.len());
        for source in inventory.files {
            check_cancelled(cancelled)?;
            let candidate = self.root.join(&source.path);
            match fs::symlink_metadata(&candidate) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => inventory.skipped += 1,
                Ok(metadata)
                    if metadata.is_file()
                        && metadata.len() <= SOURCE_LIMIT
                        && fs::canonicalize(&candidate).is_ok_and(|path| path == candidate) =>
                {
                    files.push(source);
                }
                Ok(_) => inventory.skipped += 1,
            }
        }
        inventory.files = files;
        Ok(inventory)
    }

    pub fn worktree_changes(
        &self,
        base: &str,
        dependency_mode: DependencyMode,
        cancelled: &AtomicBool,
    ) -> Result<WorktreeChanges, String> {
        validate_base(base)?;
        let revision = format!("{base}^{{commit}}");
        let capture = |include_untracked_patch| {
            thread::scope(|scope| {
                let untracked = scope.spawn(|| {
                    let output = run(
                        &self.root,
                        &["ls-files", "--others", "--exclude-standard", "-z"],
                        cancelled,
                    )?;
                    capture_untracked(
                        &self.root,
                        &output,
                        dependency_mode,
                        include_untracked_patch,
                        cancelled,
                    )
                });
                let inventory = scope.spawn(|| {
                    run(
                        &self.root,
                        &[
                            "diff-index",
                            "--raw",
                            "-z",
                            "--abbrev=64",
                            "--no-renames",
                            "--diff-filter=AMDTU",
                            "--no-color",
                            "--no-ext-diff",
                            "--no-textconv",
                            "--ignore-submodules=none",
                            &revision,
                        ],
                        cancelled,
                    )
                });
                let mut tracked_args = vec![
                    "diff-index",
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
                    &revision,
                    "--",
                    "*.rs",
                    "*.py",
                ];
                if dependency_mode == DependencyMode::Boundary {
                    tracked_args.push(":(glob,exclude).cargo/vendor/*/**");
                }
                let mut artifact_args = vec![
                    "diff-index",
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
                    revision.as_str(),
                    "--",
                    ".",
                    ":(exclude)*.rs",
                    ":(exclude)*.py",
                ];
                if dependency_mode == DependencyMode::Boundary {
                    artifact_args.push(":(glob,exclude).cargo/vendor/*/**");
                }
                let artifacts = scope.spawn(move || {
                    let output = run(&self.root, &artifact_args, cancelled)?;
                    capture_tracked_artifacts(&self.root, &output, cancelled)
                });
                let tracked = run(&self.root, &tracked_args, cancelled);
                let untracked = untracked
                    .join()
                    .map_err(|_| "Git inventory worker panicked".to_owned())?;
                let inventory = inventory
                    .join()
                    .map_err(|_| "Git metadata worker panicked".to_owned())?;
                let artifacts = artifacts
                    .join()
                    .map_err(|_| "Git artifact worker panicked".to_owned())?;
                Ok::<_, String>(WorktreeCapture {
                    tracked: tracked?,
                    artifacts: artifacts?,
                    inventory: inventory?,
                    untracked: untracked?,
                })
            })
        };
        // ponytail: two stable samples reject ordinary concurrent edits; use a
        // filesystem snapshot if adversarial ABA mutations ever matter.
        let first = capture(false)?;
        let signature = worktree_signature(&first);
        drop(first);
        let outputs = capture(true)?;
        if signature != worktree_signature(&outputs) {
            return Err("Git working tree changed while reading; retry".into());
        }
        let tracked = parse_tracked_changes(&outputs.tracked, cancelled)?;
        let inventory = parse_change_inventory(&outputs.inventory, cancelled)?;
        check_cancelled(cancelled)?;
        merge_changes(
            tracked,
            outputs.artifacts,
            inventory,
            outputs.untracked,
            dependency_mode,
            cancelled,
        )
    }

    pub fn read_source(
        &self,
        source: &SourceFile,
        cancelled: &AtomicBool,
    ) -> Result<Option<Source>, String> {
        check_cancelled(cancelled)?;
        if language_for_path(&source.path) != Some(source.language) {
            return Ok(None);
        }
        let Some(content) = read_regular_file(&self.root, &source.path, SOURCE_LIMIT, cancelled)?
        else {
            return Ok(None);
        };
        let Ok(text) = String::from_utf8(content) else {
            return Ok(None);
        };
        Ok(Some(Source {
            path: source.path.clone(),
            text,
        }))
    }
}

fn worktree_signature(outputs: &WorktreeCapture) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    for output in [&outputs.tracked, &outputs.inventory] {
        hash.update(&(output.len() as u64).to_le_bytes());
        hash.update(output);
    }
    hash.update(&(outputs.artifacts.review.patch.len() as u64).to_le_bytes());
    hash.update(outputs.artifacts.review.patch.as_bytes());
    hash.update(&(outputs.artifacts.review.analysis.len() as u64).to_le_bytes());
    hash.update(outputs.artifacts.review.analysis.as_bytes());
    hash.update(&[
        u8::from(outputs.artifacts.review.is_complete()),
        u8::from(outputs.artifacts.review.analysis_complete()),
    ]);
    hash.update(&outputs.artifacts.signature);
    hash.update(&outputs.untracked.signature);
    *hash.finalize().as_bytes()
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
    let Ok(_) = file
        .by_ref()
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
            signature: *hash.finalize().as_bytes(),
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
        signature: *hash.finalize().as_bytes(),
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

fn validate_base(base: &str) -> Result<(), String> {
    if base.trim().is_empty()
        || base.len() > 256
        || base.trim_start().starts_with('-')
        || base.chars().any(char::is_control)
    {
        Err("invalid changes base".into())
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
            signature: *hash.finalize().as_bytes(),
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
        let binary = section
            .windows(b"Binary files ".len())
            .any(|window| window == b"Binary files ")
            || section
                .windows(b"GIT binary patch".len())
                .any(|window| window == b"GIT binary patch");
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
                let new = current_semantic_text(root, path, cancelled)?;
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
        signature: *hash.finalize().as_bytes(),
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
    let marker = [b"Binary files ".as_slice(), b"GIT binary patch".as_slice()]
        .into_iter()
        .filter_map(|marker| {
            section
                .windows(marker.len())
                .position(|window| window == marker)
        })
        .min()?;
    let end = section[marker..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(section.len(), |end| marker + end + 1);
    std::str::from_utf8(&section[..end]).ok()
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
        signature: _,
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
        signature: _,
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
            _ => continue,
        };
        if language_for_path(&path.path).is_some()
            || !matches!(parse_change_path(path.path.as_bytes()), Ok(Some(_)))
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

fn parse_source_files(output: &[u8]) -> Result<SourceFiles, String> {
    if !output.is_empty() && !output.ends_with(&[0]) {
        return Err("Git returned malformed file inventory".into());
    }
    let mut candidates = HashMap::<(String, Language), Option<String>>::new();
    let mut unsupported = HashSet::new();

    for record in nul_records(output) {
        if let Some(raw_path) = record.strip_prefix(b"? ") {
            let Some((path, language)) = parse_source_path(raw_path) else {
                unsupported.insert(raw_path.to_vec());
                continue;
            };
            candidates.insert((path, language), None);
            continue;
        }
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "Git returned malformed index metadata".to_owned())?;
        let raw_path = &record[tab + 1..];
        let Some((path, language)) = parse_source_path(raw_path) else {
            unsupported.insert(raw_path.to_vec());
            continue;
        };
        let fields = record[..tab]
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        if fields.len() != 4
            || fields[0].len() != 1
            || !fields[0][0].is_ascii_alphabetic()
            || fields[1].len() != 6
            || !fields[1].iter().all(u8::is_ascii_digit)
            || !valid_oid(fields[2])
            || !matches!(fields[3], b"0" | b"1" | b"2" | b"3")
        {
            return Err("Git returned malformed index metadata".into());
        }
        let git_oid = (fields[0] == b"H"
            && (fields[1] == b"100644" || fields[1] == b"100755")
            && fields[3] == b"0")
            .then(|| {
                std::str::from_utf8(fields[2])
                    .expect("validated ASCII object ID")
                    .to_owned()
            });
        candidates
            .entry((path, language))
            .and_modify(|oid| *oid = None)
            .or_insert(git_oid);
    }

    let mut files = candidates
        .into_iter()
        .map(|((path, language), git_oid)| SourceFile {
            path,
            git_oid,
            language,
        })
        .collect::<Vec<_>>();
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(SourceFiles {
        files,
        skipped: unsupported.len(),
    })
}

fn nul_records(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    input
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
}

fn parse_source_path(input: &[u8]) -> Option<(String, Language)> {
    let path = std::str::from_utf8(input).ok()?;
    language_for_path(path).map(|language| (path.to_owned(), language))
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
    } else {
        None
    }
}

fn valid_oid(oid: &[u8]) -> bool {
    matches!(oid.len(), 40 | 64) && oid.iter().all(u8::is_ascii_hexdigit)
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

fn validate_database_path(git_dir: &Path, database: &Path) -> Result<(), String> {
    let parent = database
        .parent()
        .ok_or_else(|| "database path has no parent".to_owned())?;
    if parent.exists() {
        let metadata = fs::symlink_metadata(parent)
            .map_err(|error| format!("cannot inspect database directory: {error}"))?;
        let canonical = fs::canonicalize(parent)
            .map_err(|error| format!("cannot resolve database directory: {error}"))?;
        if !metadata.is_dir() || canonical != git_dir.join("graphr") {
            return Err("database directory is not a safe Git directory".into());
        }
    }
    if database.exists() {
        let metadata = fs::symlink_metadata(database)
            .map_err(|error| format!("cannot inspect database path: {error}"))?;
        if !metadata.is_file() {
            return Err("database path is not a regular file".into());
        }
    }
    Ok(())
}

fn validate_utf8(path: &Path, label: &str) -> Result<(), String> {
    let path = path
        .to_str()
        .ok_or_else(|| format!("{label} is not valid UTF-8"))?;
    if path.chars().any(char::is_control) {
        Err(format!("{label} contains control characters"))
    } else {
        Ok(())
    }
}

fn parse_path(output: &[u8]) -> Result<PathBuf, String> {
    let value = std::str::from_utf8(output)
        .map_err(|_| "Git path is not valid UTF-8".to_owned())?
        .trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.chars().any(char::is_control) {
        Err("Git path is empty or contains control characters".into())
    } else {
        Ok(PathBuf::from(value))
    }
}

fn run(cwd: &Path, args: &[&str], cancelled: &AtomicBool) -> Result<Vec<u8>, String> {
    run_git(
        cwd,
        args,
        false,
        false,
        Instant::now() + DEADLINE,
        STDOUT_LIMIT,
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
        cancelled,
    )
}

fn run_git(
    cwd: &Path,
    args: &[&str],
    allow_diff_exit: bool,
    isolate_repository: bool,
    deadline: Instant,
    stdout_limit: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("Git cancelled".into());
    }
    if Instant::now() >= deadline {
        return Err("Git timed out".into());
    }
    let mut command = Command::new("git");
    command.args(["--no-pager", "-c", "core.fsmonitor=false"]);
    if isolate_repository {
        command.args(["-c", "core.attributesFile=/dev/null"]);
    }
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_PAGER", "cat")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
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
    if isolate_repository {
        command
            .env("GIT_DIR", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1");
    } else {
        command.env_remove("GIT_DIR");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

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
    fn inventory_keeps_non_content_statuses_unsupported() {
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
                path: "typed.txt".into(),
                language: None,
                additions: None,
                deletions: None,
            },
            ChangedPath {
                status: ChangeStatus::Unmerged,
                old_path: None,
                old_language: None,
                path: "conflict.txt".into(),
                language: None,
                additions: None,
                deletions: None,
            },
        ];
        let mut review = ArtifactReview::default();
        finalize_artifact_omissions(&paths, &mut review, DependencyMode::Boundary);
        assert_eq!(
            review.file("typed.txt").unwrap().omission,
            Some(ArtifactOmission::TypeChanged)
        );
        assert_eq!(
            review.file("conflict.txt").unwrap().omission,
            Some(ArtifactOmission::Unmerged)
        );
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
                signature: [0; 32],
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
                signature: [0; 32],
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
    fn untracked_snapshot_hashes_the_content_used_for_stats() {
        let root = temp_root("untracked-snapshot");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("fixture.tsv"), "one\n").unwrap();
        std::os::unix::fs::symlink("fixture.tsv", root.join("link.rs")).unwrap();

        let first = capture_untracked(
            &root,
            b"fixture.tsv\0link.rs\0",
            DependencyMode::Boundary,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
        fs::write(root.join("fixture.tsv"), "two\n").unwrap();
        let second = capture_untracked(
            &root,
            b"fixture.tsv\0link.rs\0",
            DependencyMode::Boundary,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(first.paths[0].additions, second.paths[0].additions);
        assert_ne!(first.signature, second.signature);
        assert_eq!(second.paths[1].language, None);
        assert_eq!(second.paths[1].additions, None);
        fs::remove_dir_all(root).unwrap();
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

        let snapshot = |inventory| WorktreeCapture {
            tracked: vec![b'a'],
            artifacts: TrackedArtifactSnapshot::default(),
            inventory,
            untracked: UntrackedSnapshot {
                paths: Vec::new(),
                source_patch: Vec::new(),
                artifacts: ArtifactReview::default(),
                skipped_paths: 0,
                signature: [0; 32],
            },
        };
        let first = snapshot(vec![b'b']);
        let second = snapshot(vec![b'B']);
        assert_ne!(worktree_signature(&first), worktree_signature(&second));
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
                    },
                    ChangedPath {
                        status: ChangeStatus::Deleted,
                        old_path: None,
                        old_language: None,
                        path: "deleted.rs".into(),
                        language: Some(Language::Rust),
                        additions: Some(0),
                        deletions: Some(1),
                    },
                    ChangedPath {
                        status: ChangeStatus::Modified,
                        old_path: None,
                        old_language: None,
                        path: "modified.rs".into(),
                        language: Some(Language::Rust),
                        additions: Some(1),
                        deletions: Some(3),
                    },
                    ChangedPath {
                        status: ChangeStatus::Renamed,
                        old_path: Some("old.rs".into()),
                        old_language: Some(Language::Rust),
                        path: "renamed.rs".into(),
                        language: Some(Language::Rust),
                        additions: Some(0),
                        deletions: Some(0),
                    },
                    ChangedPath {
                        status: ChangeStatus::Untracked,
                        old_path: None,
                        old_language: None,
                        path: "untracked.rs".into(),
                        language: Some(Language::Rust),
                        additions: Some(1),
                        deletions: Some(0),
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
    fn inventory_only_exposes_clean_regular_stage_zero_oids() {
        let output = format!(
            "H 100644 {OID} 0\tb.rs\0H 100755 {OID} 0\ta.rs\0h 100644 {OID} 0\tc.rs\0H 100644 {OID} 1\td.rs\0H 120000 {OID} 0\te.rs\0C 100755 {OID} 0\ta.rs\0? f.rs\0"
        );
        let inventory = parse_source_files(output.as_bytes()).unwrap();

        assert_eq!(
            inventory.files,
            [
                ("a.rs", None),
                ("b.rs", Some(OID)),
                ("c.rs", None),
                ("d.rs", None),
                ("e.rs", None),
                ("f.rs", None),
            ]
            .map(|(path, oid)| SourceFile {
                path: path.into(),
                git_oid: oid.map(str::to_owned),
                language: Language::Rust,
            })
        );
        assert_eq!(inventory.skipped, 0);
    }

    #[test]
    fn inventory_sorts_deduplicates_and_rejects_unsafe_paths() {
        let mut output = format!(
            "H 100644 {OID} 0\tz.rs\0H 100644 {OID} 0\tz.rs\0H 100644 {OID} 0\t../bad.rs\0? nested/a.py\0? nested/a.py\0? not-source.txt\0? bad\nname.rs\0"
        )
        .into_bytes();
        output.extend_from_slice(b"? \xff.rs\0");
        let inventory = parse_source_files(&output).unwrap();

        assert_eq!(
            inventory.files,
            [
                SourceFile {
                    path: "nested/a.py".into(),
                    git_oid: None,
                    language: Language::Python,
                },
                SourceFile {
                    path: "z.rs".into(),
                    git_oid: None,
                    language: Language::Rust,
                },
            ]
        );
        assert_eq!(inventory.skipped, 4);
        assert!(parse_source_files(b"broken").is_err());
        assert!(parse_source_files(b"broken\0").is_err());
    }

    #[test]
    fn git_inventory_and_secure_reader_cover_clean_dirty_and_untracked() {
        let root = temp_root("inventory");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/clean.rs"), "fn clean() {}\n").unwrap();
        fs::write(root.join("src/dirty.rs"), "fn before() {}\n").unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["add", "--", "src/clean.rs", "src/dirty.rs"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("src/dirty.rs"), "fn after() {}\n").unwrap();
        fs::write(root.join("src/untracked.rs"), "fn untracked() {}\n").unwrap();
        fs::write(root.join("src/untracked.py"), "def untracked(): pass\n").unwrap();

        let repository = Repository {
            root: fs::canonicalize(&root).unwrap(),
            database: root.join(".git/graphr/index.db"),
        };
        let cancelled = AtomicBool::new(false);
        let inventory = repository.source_files(&cancelled).unwrap();
        assert_eq!(
            inventory
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.git_oid.is_some()))
                .collect::<Vec<_>>(),
            [
                ("src/clean.rs", true),
                ("src/dirty.rs", false),
                ("src/untracked.py", false),
                ("src/untracked.rs", false),
            ]
        );
        let dirty = inventory
            .files
            .iter()
            .find(|file| file.path == "src/dirty.rs")
            .unwrap();
        assert_eq!(
            repository
                .read_source(dirty, &cancelled)
                .unwrap()
                .unwrap()
                .text,
            "fn after() {}\n"
        );

        fs::remove_dir_all(root).unwrap();
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

        let repository = Repository {
            root: fs::canonicalize(&root).unwrap(),
            database: root.join(".git/graphr/index.db"),
        };
        let changes = repository
            .worktree_changes("HEAD", DependencyMode::Boundary, &AtomicBool::new(false))
            .unwrap();

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

        let repository = Repository {
            root: fs::canonicalize(&root).unwrap(),
            database: root.join(".git/graphr/index.db"),
        };
        let changes = repository
            .worktree_changes("HEAD", DependencyMode::Boundary, &AtomicBool::new(false))
            .unwrap();

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

        let changes = Repository {
            root: fs::canonicalize(&root).unwrap(),
            database: root.join(".git/graphr/index.db"),
        }
        .worktree_changes("HEAD", DependencyMode::Boundary, &AtomicBool::new(false))
        .unwrap();

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

        let changes = Repository {
            root: fs::canonicalize(&root).unwrap(),
            database: root.join(".git/graphr/index.db"),
        }
        .worktree_changes("HEAD", DependencyMode::Boundary, &AtomicBool::new(false))
        .unwrap();

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

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "graphr-git-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
