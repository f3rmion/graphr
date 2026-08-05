use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

const STDOUT_LIMIT: usize = 64 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);
const SOURCE_LIMIT: u64 = 2 * 1024 * 1024;
const UNTRACKED_STATS_FILE_LIMIT: usize = 256;
const UNTRACKED_STATS_BYTE_LIMIT: u64 = SOURCE_LIMIT;

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

#[derive(Debug, Eq, PartialEq)]
pub struct WorktreeChanges {
    pub files: Vec<ChangedFile>,
    pub records: Vec<PathRecord>,
    pub paths: Vec<ChangedPath>,
    pub patch: String,
    pub skipped_paths: usize,
}

struct WorktreeCapture {
    tracked: Vec<u8>,
    inventory: Vec<u8>,
    untracked: UntrackedSnapshot,
}

struct UntrackedSnapshot {
    paths: Vec<ChangedPath>,
    skipped_paths: usize,
    signature: [u8; 32],
}

impl WorktreeChanges {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.records.is_empty()
            && self.paths.is_empty()
            && self.patch.is_empty()
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
        cancelled: &AtomicBool,
    ) -> Result<WorktreeChanges, String> {
        validate_base(base)?;
        let revision = format!("{base}^{{commit}}");
        let capture = || {
            thread::scope(|scope| {
                let untracked = scope.spawn(|| {
                    let output = run(
                        &self.root,
                        &["ls-files", "--others", "--exclude-standard", "-z"],
                        cancelled,
                    )?;
                    capture_untracked(&self.root, &output, cancelled)
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
                let tracked = run(
                    &self.root,
                    &[
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
                    ],
                    cancelled,
                );
                let untracked = untracked
                    .join()
                    .map_err(|_| "Git inventory worker panicked".to_owned())?;
                let inventory = inventory
                    .join()
                    .map_err(|_| "Git metadata worker panicked".to_owned())?;
                Ok::<_, String>(WorktreeCapture {
                    tracked: tracked?,
                    inventory: inventory?,
                    untracked: untracked?,
                })
            })
        };
        // ponytail: two stable samples reject ordinary concurrent edits; use a
        // filesystem snapshot if adversarial ABA mutations ever matter.
        let first = capture()?;
        let signature = worktree_signature(&first);
        drop(first);
        let outputs = capture()?;
        if signature != worktree_signature(&outputs) {
            return Err("Git working tree changed while reading; retry".into());
        }
        let tracked = parse_tracked_changes(&outputs.tracked, cancelled)?;
        let inventory = parse_change_inventory(&outputs.inventory, cancelled)?;
        check_cancelled(cancelled)?;
        merge_changes(tracked, inventory, outputs.untracked, cancelled)
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
    cancelled: &AtomicBool,
) -> Result<UntrackedSnapshot, String> {
    if !input.is_empty() && !input.ends_with(&[0]) {
        return Err("Git returned malformed untracked paths".into());
    }
    let mut hash = blake3::Hasher::new();
    hash.update(&(input.len() as u64).to_le_bytes());
    hash.update(input);
    let mut paths = Vec::new();
    let mut skipped_paths = 0;
    let mut files_left = UNTRACKED_STATS_FILE_LIMIT;
    let mut bytes_left = UNTRACKED_STATS_BYTE_LIMIT;
    if input.is_empty() {
        return Ok(UntrackedSnapshot {
            paths,
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
        let before = safe_regular_metadata(root, &path);
        let content = if before.is_some() && files_left > 0 && bytes_left > 0 {
            files_left -= 1;
            // ponytail: sample at most 256 files/2 MiB; raise these caps only if
            // exact all-untracked statistics justify the filesystem exposure.
            read_regular_file(root, &path, bytes_left, cancelled)?
        } else {
            None
        };
        let after = safe_regular_metadata(root, &path);
        let regular = match (&before, &after) {
            (Some(before), Some(after)) if same_file_version(before, after) => true,
            (None, None) => false,
            _ => return Err(format!("file changed while reading: {path}")),
        };
        hash.update(&[u8::from(regular)]);
        if let Some(metadata) = &after {
            hash_file_version(&mut hash, metadata);
        }
        let (additions, deletions) = if let Some(content) = content {
            bytes_left -= content.len() as u64;
            hash.update(&(content.len() as u64).to_le_bytes());
            hash.update(&content);
            if content.contains(&0) || std::str::from_utf8(&content).is_err() {
                (None, None)
            } else {
                let lines = content.iter().filter(|byte| **byte == b'\n').count()
                    + usize::from(!content.is_empty() && !content.ends_with(b"\n"));
                let lines = u64::try_from(lines)
                    .map_err(|_| "untracked line count exceeds range".to_owned())?;
                (Some(lines), Some(0))
            }
        } else {
            hash.update(&u64::MAX.to_le_bytes());
            (None, None)
        };
        paths.push(ChangedPath {
            status: ChangeStatus::Untracked,
            old_path: None,
            old_language: None,
            path: path.clone(),
            language: regular.then(|| language_for_path(&path)).flatten(),
            additions,
            deletions,
        });
    }
    Ok(UntrackedSnapshot {
        paths,
        skipped_paths,
        signature: *hash.finalize().as_bytes(),
    })
}

fn safe_regular_metadata(root: &Path, path: &str) -> Option<fs::Metadata> {
    let candidate = root.join(path);
    let metadata = fs::symlink_metadata(&candidate).ok()?;
    (metadata.is_file()
        && fs::canonicalize(&candidate).is_ok_and(|canonical| canonical == candidate))
    .then_some(metadata)
}

fn hash_file_version(hash: &mut blake3::Hasher, metadata: &fs::Metadata) {
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
    old_regular: bool,
    new_regular: bool,
}

struct RawHeader {
    kind: RawKind,
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
enum SupportedProjection {
    Current(String),
    Deleted(String),
    Renamed(String, String),
}

fn validate_supported_projection(
    files: &[ChangedFile],
    records: &[PathRecord],
    paths: &[ChangedPath],
) -> Result<(), String> {
    let mut actual = files
        .iter()
        .map(|file| SupportedProjection::Current(file.path.clone()))
        .collect::<Vec<_>>();
    actual.extend(records.iter().filter_map(|record| match record {
        PathRecord::Deleted(path) => Some(SupportedProjection::Deleted(path.clone())),
        PathRecord::Renamed(old, new) => {
            Some(SupportedProjection::Renamed(old.clone(), new.clone()))
        }
        PathRecord::Untracked(_) => None,
    }));

    let mut expected = Vec::new();
    for path in paths {
        match path.status {
            ChangeStatus::Added | ChangeStatus::Modified if path.language.is_some() => {
                expected.push(SupportedProjection::Current(path.path.clone()));
            }
            ChangeStatus::Deleted if path.language.is_some() => {
                expected.push(SupportedProjection::Deleted(path.path.clone()));
            }
            ChangeStatus::Renamed => {
                let old = path.old_path.as_deref();
                let old_supported = path.old_language.is_some();
                let new_supported = path.language.is_some();
                match (old, old_supported, new_supported) {
                    (Some(old), true, true) => {
                        expected.push(SupportedProjection::Current(path.path.clone()));
                        expected.push(SupportedProjection::Renamed(
                            old.to_owned(),
                            path.path.clone(),
                        ));
                    }
                    (Some(old), true, false) => {
                        expected.push(SupportedProjection::Deleted(old.to_owned()));
                    }
                    (_, false, true) => {
                        expected.push(SupportedProjection::Current(path.path.clone()));
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

fn apply_supported_stats(
    paths: &mut [ChangedPath],
    stats: Vec<TrackedStat>,
    mut omitted: HashSet<String>,
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
        let key = match path.status {
            ChangeStatus::Added | ChangeStatus::Modified | ChangeStatus::Deleted
                if path.language.is_some() =>
            {
                Some(path.path.as_str())
            }
            ChangeStatus::Renamed if path.language.is_some() => Some(path.path.as_str()),
            ChangeStatus::Renamed if path.old_language.is_some() => path.old_path.as_deref(),
            _ => None,
        };
        if let Some(key) = key {
            if let Some((additions, deletions)) = by_path.remove(key) {
                path.additions = Some(additions);
                path.deletions = Some(deletions);
            } else if !omitted.remove(key) {
                return Err("Git change inventories disagree; retry".into());
            }
        }
    }
    if by_path.is_empty() && omitted.is_empty() {
        Ok(())
    } else {
        Err("Git change inventories disagree; retry".into())
    }
}

fn coalesce_supported_renames(
    paths: &mut Vec<ChangedPath>,
    records: &[PathRecord],
) -> Result<(), String> {
    let mut consumed = HashSet::new();
    let mut renames = Vec::new();
    for record in records {
        let PathRecord::Renamed(old, new) = record else {
            continue;
        };
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
    (mut paths, mut skipped_paths): (Vec<ChangedPath>, usize),
    untracked: UntrackedSnapshot,
    cancelled: &AtomicBool,
) -> Result<WorktreeChanges, String> {
    let TrackedChanges {
        mut files,
        mut records,
        patch,
        stats,
        omitted_stats,
    } = tracked;
    coalesce_supported_renames(&mut paths, &records)?;
    validate_supported_projection(&files, &records, &paths)?;
    apply_supported_stats(&mut paths, stats, omitted_stats)?;
    skipped_paths += untracked.skipped_paths;
    for (index, path) in untracked.paths.into_iter().enumerate() {
        check_progress(index, cancelled)?;
        if path.language.is_some() {
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
    Ok(WorktreeChanges {
        files: merged,
        records,
        paths,
        patch,
        skipped_paths,
    })
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
    left.dev() == right.dev()
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
    if cancelled.load(Ordering::Relaxed) {
        return Err("Git cancelled".into());
    }
    let mut child = Command::new("git")
        .args(["--no-pager", "-c", "core.fsmonitor=false", "-C"])
        .arg(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_PAGER", "cat")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_LITERAL_PATHSPECS")
        .env_remove("GIT_GLOB_PATHSPECS")
        .env_remove("GIT_NOGLOB_PATHSPECS")
        .env_remove("GIT_ICASE_PATHSPECS")
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
        move || read_capped(stdout, STDOUT_LIMIT, overflow_tx)
    });
    let stderr_thread = thread::spawn(move || read_capped(stderr, STDERR_LIMIT, overflow_tx));

    let started = Instant::now();
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
        if started.elapsed() >= DEADLINE {
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
    if status.success() {
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
    }

    #[test]
    fn diagnostics_are_terminal_safe() {
        let value = sanitize(b"bad\n\x1b[31m");
        assert!(!value.chars().any(char::is_control));
        assert!(value.len() <= 512);
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
        validate_supported_projection(&files, &records, &paths).unwrap();
        apply_supported_stats(
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
            parse_change_inventory(&inventory, &AtomicBool::new(false)).unwrap(),
            UntrackedSnapshot {
                paths: Vec::new(),
                skipped_paths: 0,
                signature: [0; 32],
            },
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(changes.patch.is_empty());
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

        let first =
            capture_untracked(&root, b"fixture.tsv\0link.rs\0", &AtomicBool::new(false)).unwrap();
        fs::write(root.join("fixture.tsv"), "two\n").unwrap();
        let second =
            capture_untracked(&root, b"fixture.tsv\0link.rs\0", &AtomicBool::new(false)).unwrap();

        assert_eq!(first.paths[0].additions, second.paths[0].additions);
        assert_ne!(first.signature, second.signature);
        assert_eq!(second.paths[1].language, None);
        assert_eq!(second.paths[1].additions, None);
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
        assert!(validate_supported_projection(&[], &[], &[path]).is_err());

        let changes = WorktreeChanges {
            files: Vec::new(),
            records: Vec::new(),
            paths: Vec::new(),
            patch: "diff --git a/changed.rs b/changed.rs\n".into(),
            skipped_paths: 0,
        };
        assert!(!changes.is_empty());

        let snapshot = |inventory| WorktreeCapture {
            tracked: vec![b'a'],
            inventory,
            untracked: UntrackedSnapshot {
                paths: Vec::new(),
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
        fs::write(root.join("untracked.rs"), "fn untracked() {}\n").unwrap();
        let changes = merge_changes(
            parse_tracked_changes(tracked.as_bytes(), &cancelled).unwrap(),
            parse_change_inventory(inventory.as_bytes(), &cancelled).unwrap(),
            capture_untracked(&root, b"untracked.rs\0", &cancelled).unwrap(),
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
                patch: tracked.split_once("\0\0").unwrap().1.into(),
                skipped_paths: 0,
            }
        );
        assert!(changes.patch.contains("-old\n+new\n"));
        assert!(changes.patch.contains("rename from old.rs"));
        fs::remove_dir_all(root).unwrap();
        assert!(
            parse_tracked_changes(&tracked.as_bytes()[..tracked.len() - 1], &cancelled).is_err()
        );
        assert!(capture_untracked(&temp_root("missing"), b"a.rs\0\0", &cancelled).is_err());
        let skipped = merge_changes(
            TrackedChanges {
                files: Vec::new(),
                records: Vec::new(),
                patch: String::new(),
                stats: Vec::new(),
                omitted_stats: HashSet::new(),
            },
            (Vec::new(), 0),
            capture_untracked(&temp_root("missing"), b"\xff.rs\0", &cancelled).unwrap(),
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
    fn change_inventory_reports_unsupported_untracked_files_and_ignores_ignored_files() {
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
        std::os::unix::fs::symlink("alias-registry.v1.tsv", root.join("tests/fixtures/link.rs"))
            .unwrap();
        fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
        fs::write(root.join("bad\nname.txt"), "unsafe\n").unwrap();

        let repository = Repository {
            root: fs::canonicalize(&root).unwrap(),
            database: root.join(".git/graphr/index.db"),
        };
        let changes = repository
            .worktree_changes("HEAD", &AtomicBool::new(false))
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
                    None,
                    None,
                ),
            ]
        );
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].path, "src/domain.rs");
        assert!(changes.patch.contains("+pub fn added() {}"));
        assert!(!changes.patch.contains("tracked.tsv"));

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
