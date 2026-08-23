use std::fs;
use std::path::Path;
use std::process::Command;

const USAGE: &str = "Usage:\n  graphr serve --allow-root PATH [--allow-root PATH ...]\n  graphr index --worktree-root PATH --base REF --head REF --target commit|index|worktree [--include-untracked] [--dependency-mode boundary|full] [--evidence-manifest RELATIVE_PATH]\n  graphr --version\n";

#[test]
fn version_is_stable() {
    let version = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("graphr {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(version.stderr.is_empty());
}

#[test]
fn invalid_cli_is_concise() {
    let output = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .arg("\x1b[31m\nbad")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("graphr: invalid command\n\n{USAGE}")
    );
}

#[test]
fn explicit_options_are_required_and_unique() {
    for args in [
        vec!["serve"],
        vec!["serve", "repo"],
        vec!["index"],
        vec![
            "index",
            "--worktree-root",
            ".",
            "--worktree-root",
            ".",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--target",
            "commit",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_graphr"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8(output.stderr).unwrap().ends_with(USAGE));
    }
}

#[test]
fn include_untracked_is_only_valid_for_worktree_targets() {
    let output = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args([
            "index",
            "--worktree-root",
            ".",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--target",
            "index",
            "--include-untracked",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .starts_with("graphr: --include-untracked requires --target worktree\n")
    );
}

#[test]
fn blocking_index_prints_one_json_completion() {
    let root = repository();
    let output = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args([
            "index",
            "--worktree-root",
            root.to_str().unwrap(),
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--target",
            "commit",
            "--dependency-mode",
            "boundary",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    let completion: rmcp::serde_json::Value =
        rmcp::serde_json::from_slice(output.stdout.trim_ascii()).unwrap();
    assert_eq!(
        completion["provenance"]["worktree_root"],
        root.to_str().unwrap()
    );
    assert_eq!(completion["provenance"]["target_state"]["kind"], "commit");
    assert_eq!(completion["snapshot_id"].as_str().unwrap().len(), 64);
    fs::remove_dir_all(root).unwrap();
}

fn repository() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "graphr-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    git(&root, &["config", "user.name", "Graphr Test"]);
    git(&root, &["config", "user.email", "graphr@example.invalid"]);
    fs::write(root.join("src/lib.rs"), "pub fn indexed() {}\n").unwrap();
    git(&root, &["add", "--", "."]);
    git(&root, &["commit", "--quiet", "-m", "baseline"]);
    fs::canonicalize(root).unwrap()
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
}
