mod artifact;
mod git;
mod index;
mod mcp;
mod parse;
mod python;
mod store;
pub mod workspace;

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;

use index::Project;

const USAGE: &str = "Usage:
  graphr index [PATH] [--rebuild]
  graphr serve [PATH]
  graphr --version";

enum Action {
    Version,
    Index { path: PathBuf, rebuild: bool },
    Serve { path: PathBuf },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let action = match parse_args(env::args_os().skip(1)) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("graphr: {error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let result = match action {
        Action::Version => {
            println!("graphr {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Action::Index { path, rebuild } => Project::open(&path).and_then(|p| p.index(rebuild)),
        Action::Serve { path } => serve(path).await,
    };

    match result {
        Ok(output) => {
            if !output.is_empty() {
                println!("{output}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("graphr: {}", terminal_safe(&error));
            ExitCode::FAILURE
        }
    }
}

async fn serve(path: PathBuf) -> Result<String, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watcher = thread::Builder::new()
        .name("graphr-stdin".into())
        .spawn({
            let cancelled = cancelled.clone();
            let done = done.clone();
            move || watch_stdin_hup(&cancelled, &done)
        })
        .map_err(|error| format!("cannot watch stdin: {error}"))?;
    let project = Project::open_cancelled(&path, &cancelled).and_then(|project| {
        project
            .index_cancelled(false, cancelled.clone())
            .map(|_| project)
    });
    done.store(true, Ordering::Release);
    watcher
        .join()
        .map_err(|_| "stdin watcher failed".to_owned())?;
    let project = project?;
    mcp::serve(project).await?;
    Ok(String::new())
}

fn watch_stdin_hup(cancelled: &AtomicBool, done: &AtomicBool) {
    let mut input = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: 0,
        revents: 0,
    };
    while !done.load(Ordering::Acquire) && !cancelled.load(Ordering::Relaxed) {
        input.revents = 0;
        let result = unsafe { libc::poll(&mut input, 1, 10) };
        if result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            cancelled.store(true, Ordering::Relaxed);
            return;
        }
        if input.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            cancelled.store(true, Ordering::Relaxed);
            return;
        }
    }
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Action, &'static str> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Err("missing command");
    };

    if command == "--version" {
        return if args.next().is_none() {
            Ok(Action::Version)
        } else {
            Err("unexpected argument")
        };
    }

    match command.to_str() {
        Some("index") => parse_index(args),
        Some("serve") => parse_serve(args),
        _ => Err("invalid command"),
    }
}

fn parse_index(args: impl Iterator<Item = OsString>) -> Result<Action, &'static str> {
    let mut path = None;
    let mut rebuild = false;
    for arg in args {
        if arg == "--rebuild" {
            if rebuild {
                return Err("duplicate --rebuild");
            }
            rebuild = true;
        } else if arg.to_string_lossy().starts_with('-') {
            return Err("invalid index option");
        } else if path.replace(PathBuf::from(arg)).is_some() {
            return Err("index accepts one PATH");
        }
    }
    Ok(Action::Index {
        path: path.unwrap_or_else(|| PathBuf::from(".")),
        rebuild,
    })
}

fn parse_serve(args: impl Iterator<Item = OsString>) -> Result<Action, &'static str> {
    let mut path = None;
    for arg in args {
        if arg.to_string_lossy().starts_with('-') {
            return Err("invalid serve option");
        }
        if path.replace(PathBuf::from(arg)).is_some() {
            return Err("serve accepts one PATH");
        }
    }
    Ok(Action::Serve {
        path: path.unwrap_or_else(|| PathBuf::from(".")),
    })
}

fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .flat_map(char::escape_default)
        .take(240)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_public_cli() {
        assert!(matches!(
            parse_args(["index".into(), "repo".into(), "--rebuild".into()]),
            Ok(Action::Index { rebuild: true, .. })
        ));
        assert!(matches!(
            parse_args(["serve".into()]),
            Ok(Action::Serve { .. })
        ));
        assert!(parse_args(["serve".into(), "--rebuild".into()]).is_err());
    }
}
