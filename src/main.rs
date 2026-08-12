mod artifact;
mod git;
mod index;
pub mod job;
mod mcp;
mod parse;
mod python;
mod store;
pub mod workspace;

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, atomic::AtomicBool};

use git::DependencyMode;
use index::Engine;
use workspace::{AllowedRoots, IndexRequest, SnapshotTarget, resolve_request};

const USAGE: &str = "Usage:
  graphr serve --allow-root PATH [--allow-root PATH ...]
  graphr index --worktree-root PATH --base REF --head REF --target commit|index|worktree [--include-untracked] [--dependency-mode boundary|full]
  graphr --version";

#[derive(Debug, Eq, PartialEq)]
enum Action {
    Version,
    Index {
        worktree_root: PathBuf,
        base: String,
        head: String,
        target: SnapshotTarget,
        dependency_mode: DependencyMode,
    },
    Serve {
        allowed_roots: Vec<PathBuf>,
    },
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
        Action::Index {
            worktree_root,
            base,
            head,
            target,
            dependency_mode,
        } => run_index(worktree_root, base, head, target, dependency_mode),
        Action::Serve { allowed_roots } => serve(allowed_roots).await,
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

fn run_index(
    worktree_root: PathBuf,
    base: String,
    head: String,
    target: SnapshotTarget,
    dependency_mode: DependencyMode,
) -> Result<String, String> {
    let cancelled = AtomicBool::new(false);
    let roots = Arc::new(AllowedRoots::new(vec![worktree_root.clone()]).map_err(operation_error)?);
    let engine = Engine::new(roots);
    let request = resolve_request(
        engine.roots(),
        IndexRequest {
            worktree_root,
            base_ref: base,
            head_ref: head,
            target,
            dependency_mode,
        },
        &cancelled,
    )
    .map_err(operation_error)?;
    let completion = engine
        .build_snapshot(request, &cancelled, |_| {})
        .map_err(operation_error)?;
    rmcp::serde_json::to_string(&completion)
        .map_err(|error| format!("cannot serialize index completion: {error}"))
}

async fn serve(allowed_roots: Vec<PathBuf>) -> Result<String, String> {
    let roots = Arc::new(AllowedRoots::new(allowed_roots).map_err(operation_error)?);
    mcp::serve(Arc::new(Engine::new(roots))).await?;
    Ok(String::new())
}

fn operation_error(error: workspace::OperationError) -> String {
    error.message
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

fn parse_index(mut args: impl Iterator<Item = OsString>) -> Result<Action, &'static str> {
    let mut worktree_root = None;
    let mut base = None;
    let mut head = None;
    let mut target = None;
    let mut dependency_mode = None;
    let mut include_untracked = false;
    while let Some(option) = args.next() {
        match option.to_str() {
            Some("--worktree-root") => set_once(
                &mut worktree_root,
                PathBuf::from(next_value(&mut args, "missing --worktree-root value")?),
                "duplicate --worktree-root",
            )?,
            Some("--base") => set_once(
                &mut base,
                utf8(next_value(&mut args, "missing --base value")?)?,
                "duplicate --base",
            )?,
            Some("--head") => set_once(
                &mut head,
                utf8(next_value(&mut args, "missing --head value")?)?,
                "duplicate --head",
            )?,
            Some("--target") => set_once(
                &mut target,
                parse_target(&utf8(next_value(&mut args, "missing --target value")?)?)?,
                "duplicate --target",
            )?,
            Some("--dependency-mode") => set_once(
                &mut dependency_mode,
                parse_dependency_mode(&utf8(next_value(
                    &mut args,
                    "missing --dependency-mode value",
                )?)?)?,
                "duplicate --dependency-mode",
            )?,
            Some("--include-untracked") if !include_untracked => include_untracked = true,
            Some("--include-untracked") => return Err("duplicate --include-untracked"),
            _ => return Err("invalid index option"),
        }
    }
    let mut target = target.ok_or("missing --target")?;
    if include_untracked {
        match &mut target {
            SnapshotTarget::Worktree { include_untracked } => *include_untracked = true,
            _ => return Err("--include-untracked requires --target worktree"),
        }
    }
    Ok(Action::Index {
        worktree_root: worktree_root.ok_or("missing --worktree-root")?,
        base: base.ok_or("missing --base")?,
        head: head.ok_or("missing --head")?,
        target,
        dependency_mode: dependency_mode.unwrap_or_default(),
    })
}

fn parse_serve(mut args: impl Iterator<Item = OsString>) -> Result<Action, &'static str> {
    let mut allowed_roots = Vec::new();
    while let Some(option) = args.next() {
        if option != "--allow-root" {
            return Err("invalid serve option");
        }
        allowed_roots.push(PathBuf::from(next_value(
            &mut args,
            "missing --allow-root value",
        )?));
    }
    if allowed_roots.is_empty() {
        return Err("missing --allow-root");
    }
    Ok(Action::Serve { allowed_roots })
}

fn next_value(
    args: &mut impl Iterator<Item = OsString>,
    missing: &'static str,
) -> Result<OsString, &'static str> {
    args.next().ok_or(missing)
}

fn utf8(value: OsString) -> Result<String, &'static str> {
    value.into_string().map_err(|_| "option value is not UTF-8")
}

fn set_once<T>(
    slot: &mut Option<T>,
    value: T,
    duplicate: &'static str,
) -> Result<(), &'static str> {
    if slot.replace(value).is_some() {
        Err(duplicate)
    } else {
        Ok(())
    }
}

fn parse_target(value: &str) -> Result<SnapshotTarget, &'static str> {
    match value {
        "commit" => Ok(SnapshotTarget::Commit),
        "index" => Ok(SnapshotTarget::Index),
        "worktree" => Ok(SnapshotTarget::Worktree {
            include_untracked: false,
        }),
        _ => Err("--target must be commit, index, or worktree"),
    }
}

fn parse_dependency_mode(value: &str) -> Result<DependencyMode, &'static str> {
    match value {
        "boundary" => Ok(DependencyMode::Boundary),
        "full" => Ok(DependencyMode::Full),
        _ => Err("--dependency-mode must be boundary or full"),
    }
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
        assert_eq!(
            parse_args([
                "serve".into(),
                "--allow-root".into(),
                "/one".into(),
                "--allow-root".into(),
                "/two".into(),
            ]),
            Ok(Action::Serve {
                allowed_roots: vec!["/one".into(), "/two".into()]
            })
        );
        assert!(matches!(
            parse_args([
                "index".into(),
                "--worktree-root".into(),
                "/repo".into(),
                "--base".into(),
                "main".into(),
                "--head".into(),
                "HEAD".into(),
                "--target".into(),
                "worktree".into(),
                "--include-untracked".into(),
            ]),
            Ok(Action::Index {
                target: SnapshotTarget::Worktree {
                    include_untracked: true
                },
                ..
            })
        ));
        assert!(parse_args(["serve".into()]).is_err());
        assert!(parse_args(["serve".into(), "repo".into()]).is_err());
    }
}
