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

pub struct SourceCounts {
    pub indexed: usize,
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

    pub fn visit_rust_sources(
        &self,
        cancelled: &AtomicBool,
        mut visit: impl FnMut(Source) -> Result<(), String>,
    ) -> Result<SourceCounts, String> {
        let output = run(
            &self.root,
            &["ls-files", "-co", "--exclude-standard", "-z", "--", "*.rs"],
            cancelled,
        )?;
        let mut paths = output
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .collect::<Vec<_>>();
        paths.sort_unstable();
        paths.dedup();

        let mut indexed = 0;
        let mut skipped = 0;
        for bytes in paths {
            if cancelled.load(Ordering::Relaxed) {
                return Err("index cancelled".into());
            }
            let Ok(path) = std::str::from_utf8(bytes) else {
                skipped += 1;
                continue;
            };
            let relative = Path::new(path);
            if path.chars().any(char::is_control)
                || relative.is_absolute()
                || !relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
            {
                skipped += 1;
                continue;
            }

            let candidate = self.root.join(relative);
            let Ok(before) = fs::symlink_metadata(&candidate) else {
                skipped += 1;
                continue;
            };
            if !before.is_file() {
                skipped += 1;
                continue;
            }
            let Ok(canonical) = fs::canonicalize(&candidate) else {
                skipped += 1;
                continue;
            };
            let Ok(mut file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&candidate)
            else {
                skipped += 1;
                continue;
            };
            let Ok(after) = file.metadata() else {
                skipped += 1;
                continue;
            };
            if canonical != candidate
                || !after.is_file()
                || before.dev() != after.dev()
                || before.ino() != after.ino()
                || after.len() > SOURCE_LIMIT
            {
                skipped += 1;
                continue;
            }
            let mut content = Vec::with_capacity(after.len() as usize);
            let Ok(_) = file
                .by_ref()
                .take(SOURCE_LIMIT + 1)
                .read_to_end(&mut content)
            else {
                skipped += 1;
                continue;
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
            if content.len() as u64 > SOURCE_LIMIT {
                skipped += 1;
                continue;
            }
            let Ok(text) = String::from_utf8(content) else {
                skipped += 1;
                continue;
            };
            visit(Source {
                path: path.to_owned(),
                text,
            })?;
            indexed += 1;
        }
        Ok(SourceCounts { indexed, skipped })
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
}
