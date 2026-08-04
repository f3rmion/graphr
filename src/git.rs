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

pub struct Repository {
    pub root: PathBuf,
    pub database: PathBuf,
}

pub struct Source {
    pub path: String,
    pub text: String,
}

#[derive(Debug, Eq, PartialEq)]
pub struct RustFile {
    pub path: String,
    pub git_oid: Option<String>,
}

pub struct RustFiles {
    pub files: Vec<RustFile>,
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
}

#[derive(Debug, Eq, PartialEq)]
pub struct WorktreeChanges {
    pub files: Vec<ChangedFile>,
    pub records: Vec<PathRecord>,
}

impl WorktreeChanges {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.records.is_empty()
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
                "grapher/index.db",
            ],
            cancelled,
        )?)?;
        if database != git_dir.join("grapher/index.db") {
            return Err("Git returned an unsafe database path".into());
        }
        validate_database_path(&git_dir, &database)?;

        Ok(Self { root, database })
    }

    pub fn rust_files(&self, cancelled: &AtomicBool) -> Result<RustFiles, String> {
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
            ],
            cancelled,
        )?;
        check_cancelled(cancelled)?;
        let mut inventory = parse_rust_files(&output)?;
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
        let oid = parse_oid(&run(
            &self.root,
            &["rev-parse", "--verify", "--end-of-options", &revision],
            cancelled,
        )?)?;
        let tracked = parse_tracked_changes(
            &run(
                &self.root,
                &[
                    "diff",
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
                    "--no-ext-diff",
                    "--no-textconv",
                    "--ignore-submodules=all",
                    "--text",
                    &oid,
                    "--",
                    "*.rs",
                ],
                cancelled,
            )?,
            cancelled,
        )?;
        let untracked = run(
            &self.root,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                "*.rs",
            ],
            cancelled,
        )?;
        check_cancelled(cancelled)?;
        merge_changes(tracked, &untracked, cancelled)
    }

    pub fn read_rust_source(
        &self,
        source: &RustFile,
        cancelled: &AtomicBool,
    ) -> Result<Option<Source>, String> {
        check_cancelled(cancelled)?;
        if !valid_rust_path(&source.path) {
            return Ok(None);
        }
        let path = source.path.as_str();
        let candidate = self.root.join(path);
        let Ok(before) = fs::symlink_metadata(&candidate) else {
            return Ok(None);
        };
        if !before.is_file() {
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
            || after.len() > SOURCE_LIMIT
        {
            return Ok(None);
        }
        let mut content = Vec::with_capacity(after.len() as usize);
        let Ok(_) = file
            .by_ref()
            .take(SOURCE_LIMIT + 1)
            .read_to_end(&mut content)
        else {
            return Ok(None);
        };
        let finished = file
            .metadata()
            .map_err(|error| format!("cannot recheck source {path}: {error}"))?;
        let current = fs::symlink_metadata(&candidate)
            .map_err(|error| format!("cannot recheck source {path}: {error}"))?;
        if !current.is_file()
            || !same_file_version(&before, &finished)
            || !same_file_version(&finished, &current)
        {
            return Err(format!("source changed while indexing: {path}"));
        }
        check_cancelled(cancelled)?;
        if content.len() as u64 > SOURCE_LIMIT {
            return Ok(None);
        }
        let Ok(text) = String::from_utf8(content) else {
            return Ok(None);
        };
        Ok(Some(Source {
            path: source.path.clone(),
            text,
        }))
    }
}

#[derive(Clone, Copy)]
enum RawKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

struct RawChange {
    kind: RawKind,
    old: Option<String>,
    new: Option<String>,
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

fn parse_oid(output: &[u8]) -> Result<String, String> {
    let output = output.strip_suffix(b"\n").unwrap_or(output);
    let output = output.strip_suffix(b"\r").unwrap_or(output);
    if !valid_oid(output) {
        return Err("Git returned an invalid commit ID".into());
    }
    Ok(std::str::from_utf8(output)
        .expect("validated ASCII object ID")
        .to_owned())
}

fn parse_tracked_changes(
    output: &[u8],
    cancelled: &AtomicBool,
) -> Result<(Vec<ChangedFile>, Vec<PathRecord>), String> {
    if output.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let boundary = output
        .windows(2)
        .position(|bytes| bytes == b"\0\0")
        .ok_or_else(|| "Git returned malformed diff metadata".to_owned())?;
    let raw = &output[..=boundary];
    let patch = &output[boundary + 2..];
    let raw = parse_raw_changes(raw, cancelled)?;
    let hunks = parse_patch_hunks(patch, raw.len(), cancelled)?;
    let mut files = Vec::new();
    let mut records = Vec::new();

    for (index, (change, spans)) in raw.into_iter().zip(hunks).enumerate() {
        check_progress(index, cancelled)?;
        match change.kind {
            RawKind::Added => {
                if let Some(path) = change.new {
                    files.push(ChangedFile {
                        path,
                        whole_file: true,
                        spans,
                        report_unmapped: true,
                    });
                }
            }
            RawKind::Modified => {
                if let Some(path) = change.new {
                    files.push(ChangedFile {
                        path,
                        whole_file: false,
                        spans,
                        report_unmapped: true,
                    });
                }
            }
            RawKind::Deleted => {
                if let Some(path) = change.old {
                    records.push(PathRecord::Deleted(path));
                }
            }
            RawKind::Renamed => match (change.old, change.new) {
                (Some(old), Some(new)) => {
                    files.push(ChangedFile {
                        path: new.clone(),
                        whole_file: true,
                        report_unmapped: !spans.is_empty(),
                        spans,
                    });
                    records.push(PathRecord::Renamed(old, new));
                }
                (Some(old), None) => records.push(PathRecord::Deleted(old)),
                (None, Some(new)) => files.push(ChangedFile {
                    path: new,
                    whole_file: true,
                    spans,
                    report_unmapped: true,
                }),
                (None, None) => {}
            },
        }
    }
    Ok((files, records))
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
        let status = fields[4];
        let kind = match status {
            b"A" => RawKind::Added,
            b"M" => RawKind::Modified,
            b"D" => RawKind::Deleted,
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
        changes.push(RawChange { kind, old, new });
    }
    Ok(changes)
}

fn parse_patch_hunks(
    input: &[u8],
    file_count: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<Vec<LineSpan>>, String> {
    if file_count == 0 || !input.starts_with(b"diff --git ") || !input.ends_with(b"\n") {
        return Err("Git returned malformed patch metadata".into());
    }
    let mut hunks: Vec<Vec<LineSpan>> = vec![Vec::new(); file_count];
    let mut current = None;
    let mut sections = 0;
    for (index, line) in input.split(|byte| *byte == b'\n').enumerate() {
        check_progress(index, cancelled)?;
        if line.starts_with(b"diff --git ") {
            if sections == file_count {
                return Err("Git diff changed while reading it; retry".into());
            }
            current = Some(sections);
            sections += 1;
        } else if line.starts_with(b"@@ ") {
            let current =
                current.ok_or_else(|| "Git returned a hunk without file metadata".to_owned())?;
            let span = parse_hunk(line)?;
            if hunks[current]
                .last()
                .is_some_and(|previous| previous.end >= span.start)
            {
                return Err("Git returned overlapping diff hunks".into());
            }
            hunks[current].push(span);
        }
    }
    if sections != file_count {
        return Err("Git diff changed while reading it; retry".into());
    }
    Ok(hunks)
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
    (mut files, mut records): (Vec<ChangedFile>, Vec<PathRecord>),
    untracked: &[u8],
    cancelled: &AtomicBool,
) -> Result<WorktreeChanges, String> {
    if !untracked.is_empty() && !untracked.ends_with(&[0]) {
        return Err("Git returned malformed untracked paths".into());
    }
    if let Some(paths) = untracked.strip_suffix(&[0]) {
        for (index, path) in paths.split(|byte| *byte == 0).enumerate() {
            check_progress(index, cancelled)?;
            if path.is_empty() {
                return Err("Git returned malformed untracked paths".into());
            }
            if let Some(path) = parse_change_path(path)? {
                files.push(ChangedFile {
                    path,
                    whole_file: true,
                    spans: Vec::new(),
                    report_unmapped: true,
                });
            }
        }
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
    Ok(WorktreeChanges {
        files: merged,
        records,
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
    Ok(path.ends_with(".rs").then(|| path.to_owned()))
}

fn parse_rust_files(output: &[u8]) -> Result<RustFiles, String> {
    if !output.is_empty() && !output.ends_with(&[0]) {
        return Err("Git returned malformed file inventory".into());
    }
    let mut candidates = HashMap::<String, Option<String>>::new();
    let mut unsupported = HashSet::new();

    for record in nul_records(output) {
        if let Some(raw_path) = record.strip_prefix(b"? ") {
            let Some(path) = parse_rust_path(raw_path) else {
                unsupported.insert(raw_path.to_vec());
                continue;
            };
            candidates.insert(path, None);
            continue;
        }
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "Git returned malformed index metadata".to_owned())?;
        let raw_path = &record[tab + 1..];
        let Some(path) = parse_rust_path(raw_path) else {
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
            .entry(path)
            .and_modify(|oid| *oid = None)
            .or_insert(git_oid);
    }

    let mut files = candidates
        .into_iter()
        .map(|(path, git_oid)| RustFile { path, git_oid })
        .collect::<Vec<_>>();
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(RustFiles {
        files,
        skipped: unsupported.len(),
    })
}

fn nul_records(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    input
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
}

fn parse_rust_path(input: &[u8]) -> Option<String> {
    let path = std::str::from_utf8(input).ok()?;
    valid_rust_path(path).then(|| path.to_owned())
}

fn valid_rust_path(path: &str) -> bool {
    let relative = Path::new(path);
    path.ends_with(".rs")
        && !path.chars().any(char::is_control)
        && !relative.is_absolute()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
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
        if !metadata.is_dir() || canonical != git_dir.join("grapher") {
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
        let changes = merge_changes(
            parse_tracked_changes(tracked.as_bytes(), &cancelled).unwrap(),
            b"untracked.rs\0",
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
                ],
            }
        );
        assert!(
            parse_tracked_changes(&tracked.as_bytes()[..tracked.len() - 1], &cancelled).is_err()
        );
        assert!(merge_changes((Vec::new(), Vec::new()), b"a.rs\0\0", &cancelled).is_err());
        assert!(parse_change_path(b"../not-rust.txt").is_err());
    }

    #[test]
    fn detects_a_source_version_change() {
        let path =
            std::env::temp_dir().join(format!("grapher-source-version-{}", std::process::id()));
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
        let inventory = parse_rust_files(output.as_bytes()).unwrap();

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
            .map(|(path, oid)| RustFile {
                path: path.into(),
                git_oid: oid.map(str::to_owned),
            })
        );
        assert_eq!(inventory.skipped, 0);
    }

    #[test]
    fn inventory_sorts_deduplicates_and_rejects_unsafe_paths() {
        let mut output = format!(
            "H 100644 {OID} 0\tz.rs\0H 100644 {OID} 0\tz.rs\0H 100644 {OID} 0\t../bad.rs\0? nested/a.rs\0? nested/a.rs\0? not-rust.txt\0? bad\nname.rs\0"
        )
        .into_bytes();
        output.extend_from_slice(b"? \xff.rs\0");
        let inventory = parse_rust_files(&output).unwrap();

        assert_eq!(
            inventory.files,
            [
                RustFile {
                    path: "nested/a.rs".into(),
                    git_oid: None,
                },
                RustFile {
                    path: "z.rs".into(),
                    git_oid: None,
                },
            ]
        );
        assert_eq!(inventory.skipped, 4);
        assert!(parse_rust_files(b"broken").is_err());
        assert!(parse_rust_files(b"broken\0").is_err());
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

        let repository = Repository {
            root: fs::canonicalize(&root).unwrap(),
            database: root.join(".git/grapher/index.db"),
        };
        let cancelled = AtomicBool::new(false);
        let inventory = repository.rust_files(&cancelled).unwrap();
        assert_eq!(
            inventory
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.git_oid.is_some()))
                .collect::<Vec<_>>(),
            [
                ("src/clean.rs", true),
                ("src/dirty.rs", false),
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
                .read_rust_source(dirty, &cancelled)
                .unwrap()
                .unwrap()
                .text,
            "fn after() {}\n"
        );

        fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "grapher-git-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
