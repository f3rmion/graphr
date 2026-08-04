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
