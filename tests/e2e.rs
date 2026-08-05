use std::env;
use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};

#[test]
fn binary_only_crate_resolves_crate_paths() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("src/main.rs"),
        "mod worker; fn main() { crate::worker::work(); }\n",
    )
    .unwrap();
    fs::write(fixture.path.join("src/worker.rs"), "pub fn work() {}\n").unwrap();
    init_git(&fixture.path);

    let indexed = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args(["index", fixture.path.to_str().unwrap(), "--rebuild"])
        .output()
        .unwrap();
    assert!(indexed.status.success(), "{:?}", indexed.stderr);
    assert_eq!(
        Connection::open(fixture.path.join(".git/graphr/index.db"))
            .unwrap()
            .query_row("SELECT count(*) FROM edges WHERE kind='CALLS'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
}

#[test]
fn rust_attribute_only_changes_map_to_declarations() {
    const BASELINE: &str = "#[derive(Debug)]\npub struct Item;\n\n#[inline]\npub fn free_function() {}\n\npub struct Worker;\nimpl Worker {\n    #[inline]\n    pub fn method(&self) {}\n}\n\n#[ignore = \"before\"]\n#[test]\nfn test_function() {}\n";
    const EDITED: &str = "#[derive(Clone)]\npub struct Item;\n\n#[cold]\npub fn free_function() {}\n\npub struct Worker;\nimpl Worker {\n    #[cold]\n    pub fn method(&self) {}\n}\n\n#[ignore = \"after\"]\n#[test]\nfn test_function() {}\n";

    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/lib.rs"), BASELINE).unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git(
        &fixture.path,
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
    index_repository(&fixture.path, true);

    fs::write(fixture.path.join("src/lib.rs"), EDITED).unwrap();
    index_repository(&fixture.path, false);

    let mut client = Client::start(&fixture.path);
    let _ = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
    );
    client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let changes = client.request(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"changes","arguments":{"depth":0,"max_nodes":20}}}"#,
    );
    let text = response_text(&changes);
    for name in ["Item", "free_function", "method", "test_function"] {
        assert!(text.contains(name), "missing {name}: {changes}");
    }
    assert!(
        !text
            .split_once("graph\n")
            .unwrap()
            .1
            .contains("unmapped src/lib.rs"),
        "{changes}"
    );
    client.close();
}

#[test]
fn empty_trait_impl_resolves_across_files_and_retargets_incrementally() {
    const MARKER_IMPL: &str = "impl crate::api::Marker for crate::model::Item {}\nimpl crate::api::Maybe for crate::model::Item {}\npub fn call() { crate::api::maybe_call(); }\n";
    const OTHER_IMPL: &str = "impl crate::api::Other for crate::model::Item {}\nimpl crate::api::Maybe for crate::model::Item {}\npub fn call() { crate::api::maybe_call(); }\n";

    let incremental = Fixture::new();
    let oracle = Fixture::new();
    for root in [&incremental.path, &oracle.path] {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "mod api;\nmod implementations;\nmod model;\nmod traits;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/api.rs"),
            "pub use crate::traits::{Marker, Other};\n#[cfg(unix)]\npub use crate::traits::Ambiguous as Maybe;\n#[cfg(windows)]\npub use external::Ambiguous as Maybe;\n#[cfg(unix)]\npub use crate::model::helper as maybe_call;\n#[cfg(windows)]\npub use external::helper as maybe_call;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/model.rs"),
            "pub struct Item;\npub fn helper() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/traits.rs"),
            "pub trait Marker {}\npub trait Other {}\npub trait Ambiguous {}\n",
        )
        .unwrap();
        fs::write(root.join("src/implementations.rs"), MARKER_IMPL).unwrap();
        init_git(root);
        git(root, &["add", "--", "."]);
    }

    index_repository(&incremental.path, true);
    index_repository(&oracle.path, true);
    assert_eq!(
        trait_implementation_count(&incremental.path, "Item", "Marker"),
        1
    );
    assert_eq!(
        trait_implementation_count(&incremental.path, "Item", "Ambiguous"),
        0
    );
    assert_eq!(named_edge_count(&incremental.path, "call", "helper"), 0);

    let mut client = Client::start(&incremental.path);
    let _ = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
    );
    client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    for (id, query, relation, related) in [
        (2, "Item", "implements ->", "Marker"),
        (4, "Marker", "impl <-", "Item"),
    ] {
        let search = client.request(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"search","arguments":{{"query":"{query}","kind":"type"}}}}}}"#,
        ));
        let search_text = response_text(&search);
        let node_ref = search_text.split_whitespace().next().unwrap();
        let view = client.request(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{"name":"view","arguments":{{"node_ref":"{node_ref}","depth":1}}}}}}"#,
            id + 1
        ));
        assert!(view.contains(relation), "{view}");
        assert!(view.contains(related), "{view}");
    }
    client.close();

    for root in [&incremental.path, &oracle.path] {
        fs::write(
            root.join("src/traits.rs"),
            "#[allow(dead_code)]\npub trait Marker {}\n#[allow(dead_code)]\npub trait Other {}\n#[allow(dead_code)]\npub trait Ambiguous {}\n",
        )
        .unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);
    assert_eq!(
        trait_implementation_count(&incremental.path, "Item", "Marker"),
        1
    );

    for root in [&incremental.path, &oracle.path] {
        fs::write(root.join("src/implementations.rs"), OTHER_IMPL).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);
    assert_eq!(
        trait_implementation_count(&incremental.path, "Item", "Marker"),
        0
    );
    assert_eq!(
        trait_implementation_count(&incremental.path, "Item", "Other"),
        1
    );
    assert_eq!(
        trait_implementation_count(&incremental.path, "Item", "Ambiguous"),
        0
    );
    assert_eq!(named_edge_count(&incremental.path, "call", "helper"), 0);
}

#[test]
fn python_index_search_view_and_incremental_changes_over_mcp() {
    const INIT: &str = "from sample.engine import run as public_run\nfrom sample.mid import mid_run as chained_run\nfrom sample.engine import run as api\nimport sample.api as public_module\n";
    const EDITED_INIT: &str = "from sample.engine import run as public_run\nfrom sample.mid import mid_run as chained_run\nfrom sample.engine import run as api\nimport sample.api as public_module\n# changed\n";
    const CHECKS: &str = "def validate(value):\n    return value\n\ndef secret():\n    return None\n\n@first\ndef decorated():\n    return None\n";
    const EDITED_CHECKS: &str = "def validate(value):\n    return value\n\ndef secret():\n    return None\n\n@second\ndef decorated():\n    return None\n";
    const ENGINE: &str = "from sample.checks import validate as check, secret as sibling, secret as module_value\n\nmodule_value = lambda: None\n\nclass Stage:\n    check = None\n    def dispatch(self, value):\n        return check(value)\n\ndef run(value):\n    return Stage()\n\ndef module_user():\n    module_value()\n\ndef outer(check):\n    def sibling():\n        return None\n    def inner():\n        check()\n        sibling()\n        inner()\n    return inner\n";
    const EDITED_ENGINE: &str = "from sample.checks import validate as check, secret as sibling, secret as module_value\n\nmodule_value = lambda: None\n\nclass Stage:\n    check = None\n    def dispatch(self, value):\n        return check(check(value))\n\ndef run(value):\n    return Stage()\n\ndef module_user():\n    module_value()\n\ndef outer(check):\n    def sibling():\n        return None\n    def inner():\n        check()\n        sibling()\n        inner()\n    return inner\n";

    let incremental = Fixture::new();
    let oracle = Fixture::new();
    for root in [&incremental.path, &oracle.path] {
        fs::create_dir_all(root.join("src/sample")).unwrap();
        fs::create_dir(root.join("src/sample/mid")).unwrap();
        fs::create_dir(root.join("tests")).unwrap();
        fs::write(root.join("src/sample/__init__.py"), INIT).unwrap();
        fs::write(
            root.join("src/sample/mid/__init__.py"),
            "from sample.engine import run as mid_run\n",
        )
        .unwrap();
        fs::write(root.join("src/sample/api.py"), "VALUE = 1\n").unwrap();
        fs::write(root.join("src/sample/checks.py"), CHECKS).unwrap();
        fs::write(root.join("src/sample/engine.py"), ENGINE).unwrap();
        fs::write(
            root.join("tests/test_engine.py"),
            "from sample import api, public_module, public_run, secret\nfrom sample.future import later\n\ndef test_run():\n    assert public_run(1)\n    later()\n    secret()\n\ndef test_collision():\n    api()\n\ndef test_module_alias():\n    public_module()\n",
        )
        .unwrap();
        init_git(root);
        git(root, &["add", "--", "."]);
        git(
            root,
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
    }

    index_repository(&incremental.path, true);

    let database = incremental.path.join(".git/graphr/index.db");
    let connection = Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM files WHERE language='python'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        6
    );
    for (source_path, source, target_path, target, kind) in [
        (
            "src/sample/engine.py",
            "dispatch",
            "src/sample/checks.py",
            "validate",
            "CALLS",
        ),
        (
            "src/sample/engine.py",
            "run",
            "src/sample/engine.py",
            "Stage",
            "CALLS",
        ),
        (
            "tests/test_engine.py",
            "test_run",
            "src/sample/engine.py",
            "run",
            "TEST_CALLS",
        ),
    ] {
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM edges edge
                       JOIN nodes source ON source.id=edge.source_id
                       JOIN files source_file ON source_file.id=source.file_id
                       JOIN nodes target ON target.id=edge.target_id
                       JOIN files target_file ON target_file.id=target.file_id
                      WHERE source_file.path=?1 AND source.name=?2
                        AND target_file.path=?3 AND target.name=?4 AND edge.kind=?5",
                    [source_path, source, target_path, target, kind],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }
    assert_eq!(named_edge_count(&incremental.path, "test_run", "secret"), 0);
    assert_eq!(named_edge_count(&incremental.path, "inner", "validate"), 0);
    assert_eq!(named_edge_count(&incremental.path, "inner", "secret"), 0);
    assert_eq!(
        named_edge_count(&incremental.path, "module_user", "secret"),
        0
    );
    assert_eq!(named_edge_count(&incremental.path, "inner", "sibling"), 1);
    assert_eq!(named_edge_count(&incremental.path, "inner", "inner"), 1);
    assert_eq!(
        named_edge_count(&incremental.path, "test_collision", "run"),
        0
    );
    assert_eq!(
        named_edge_count(&incremental.path, "test_collision", "src/sample/api.py"),
        0
    );
    assert_eq!(
        named_edge_count(&incremental.path, "test_module_alias", "src/sample/api.py"),
        1
    );
    drop(connection);

    for root in [&incremental.path, &oracle.path] {
        fs::write(root.join("src/sample/engine.py"), EDITED_ENGINE).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);

    for root in [&incremental.path, &oracle.path] {
        fs::write(root.join("src/sample/__init__.py"), EDITED_INIT).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);

    for root in [&incremental.path, &oracle.path] {
        fs::write(
            root.join("src/sample/future.py"),
            "def later():\n    return None\n",
        )
        .unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);
    assert_eq!(named_edge_count(&incremental.path, "test_run", "later"), 1);

    for root in [&incremental.path, &oracle.path] {
        fs::rename(
            root.join("src/sample/future.py"),
            root.join("src/sample/moved.py"),
        )
        .unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 2);
    assert_eq!(named_edge_count(&incremental.path, "test_run", "later"), 0);

    for root in [&incremental.path, &oracle.path] {
        fs::rename(
            root.join("src/sample/moved.py"),
            root.join("src/sample/future.py"),
        )
        .unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 2);
    assert_eq!(named_edge_count(&incremental.path, "test_run", "later"), 1);

    for root in [&incremental.path, &oracle.path] {
        fs::remove_file(root.join("src/sample/future.py")).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);
    assert_eq!(named_edge_count(&incremental.path, "test_run", "later"), 0);

    for root in [&incremental.path, &oracle.path] {
        fs::write(root.join("src/sample/checks.py"), EDITED_CHECKS).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);

    let mut client = Client::start(&incremental.path);
    let _ = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
    );
    client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let search = client.request(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"query":"Stage","kind":"type"}}}"#,
    );
    let search_text = response_text(&search);
    let node_ref = search_text.split_whitespace().next().unwrap();
    let view = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"view","arguments":{{"node_ref":"{node_ref}","depth":2}}}}}}"#
    ));
    assert!(view.contains("member ->"), "{view}");
    assert!(view.contains("dispatch"), "{view}");
    let changes = client.request(
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"changes","arguments":{}}}"#,
    );
    assert!(changes.contains("dispatch"), "{changes}");
    assert!(changes.contains("decorated"), "{changes}");
    client.close();
}

#[test]
fn incremental_index_matches_rebuild_through_mutations() {
    const CALLER: &str = "use crate::target::answer;\npub fn call() { answer(); answer(); }\n";
    const EDITED_CALLER: &str =
        "use crate::target::answer;\npub fn call() { answer(); answer(); answer(); }\n";
    const TARGET: &str =
        "pub struct Widget;\nimpl Widget { pub fn local(&self) {} }\npub fn answer() {}\n";

    let incremental = Fixture::new();
    let oracle = Fixture::new();
    let roots = [&incremental.path, &oracle.path];
    for root in roots {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "mod caller;\nmod ext;\nmod extra;\nmod target;\n",
        )
        .unwrap();
        fs::write(root.join("src/caller.rs"), CALLER).unwrap();
        fs::write(root.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
        fs::write(
            root.join("src/ext.rs"),
            "use crate::target::Widget;\nimpl Widget { pub fn ping(&self) {} }\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("crates/app/src")).unwrap();
        fs::write(root.join("crates/app/src/worker.rs"), "pub fn work() {}\n").unwrap();
        init_git(root);
        git(root, &["add", "--", "src", "crates/app/src/worker.rs"]);
    }

    index_repository(&incremental.path, true);
    index_repository(&oracle.path, true);
    assert_eq!(
        semantic_graph(&incremental.path),
        semantic_graph(&oracle.path)
    );
    assert_resolution(&incremental.path, None, "file");

    let generation = database_generation(&incremental.path.join(".git/graphr/index.db"));
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 0);
    assert_eq!(
        database_generation(&incremental.path.join(".git/graphr/index.db")),
        generation
    );

    for root in roots {
        fs::write(
            root.join("crates/app/Cargo.toml"),
            "[package]\nname='app'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(root.join("crates/app/src/lib.rs"), "mod worker;\n").unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 2);

    for root in roots {
        fs::remove_file(root.join("crates/app/Cargo.toml")).unwrap();
        fs::remove_file(root.join("crates/app/src/lib.rs")).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 2);

    for root in roots {
        fs::write(root.join("src/target.rs"), TARGET).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);
    assert_resolution(&incremental.path, Some(2), "type");

    for root in roots {
        fs::write(root.join("src/caller.rs"), EDITED_CALLER).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);
    assert_resolution(&incremental.path, Some(3), "type");

    for root in roots {
        fs::create_dir(root.join("src/target")).unwrap();
        fs::write(root.join("src/target/mod.rs"), TARGET).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);
    assert_resolution(&incremental.path, None, "file");

    for root in roots {
        fs::remove_file(root.join("src/target/mod.rs")).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);
    assert_resolution(&incremental.path, Some(3), "type");

    for root in roots {
        fs::rename(root.join("src/target.rs"), root.join("src/moved.rs")).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 2);
    assert_resolution(&incremental.path, None, "file");

    for root in roots {
        fs::rename(root.join("src/moved.rs"), root.join("src/target.rs")).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 2);
    assert_resolution(&incremental.path, Some(3), "type");

    for root in roots {
        fs::write(root.join("src/caller.rs"), CALLER).unwrap();
    }
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 1);
    assert_resolution(&incremental.path, Some(2), "type");
}

#[test]
fn changes_maps_mixed_worktree_edits_to_current_graph() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "mod changed;\nmod moved;\nmod removed;\nuse crate::changed::target;\npub fn caller() { target(); }\n#[test]\nfn checks_target() { target(); }\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/changed.rs"),
        "pub fn target() {\n    helper();\n}\npub fn helper() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/moved.rs"),
        "pub fn moved_symbol() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/removed.rs"),
        "pub fn removed_symbol() {}\n",
    )
    .unwrap();
    fs::write(fixture.path.join(".gitignore"), "src/ignored.rs\n").unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git(
        &fixture.path,
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
    index_repository(&fixture.path, true);

    let mut client = Client::start(&fixture.path);
    let _ = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
    );
    client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let clean = client.request(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"changes","arguments":{}}}"#,
    );
    assert!(clean.contains("no changes\\n"), "{clean}");

    fs::write(
        fixture.path.join("src/changed.rs"),
        "pub fn target() {\n    helper();\n    helper();\n}\npub fn helper() {}\n",
    )
    .unwrap();
    git(&fixture.path, &["add", "--", "src/changed.rs"]);
    fs::write(
        fixture.path.join("src/changed.rs"),
        "pub fn target() {\n    helper();\n    helper();\n}\npub fn helper() { let _ = 1; }\n",
    )
    .unwrap();
    git(
        &fixture.path,
        &["mv", "--", "src/moved.rs", "src/renamed.rs"],
    );
    fs::remove_file(fixture.path.join("src/removed.rs")).unwrap();
    fs::write(
        fixture.path.join("src/untracked.rs"),
        "pub fn first_untracked() { second_untracked(); }\npub fn second_untracked() {}\n#[test]\nfn checks_untracked() { first_untracked(); }\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/ignored.rs"),
        "pub fn ignored_symbol() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "// changed outside a symbol\nmod changed;\nmod moved;\nmod removed;\nuse crate::changed::target;\npub fn caller() { target(); }\n#[test]\nfn checks_target() { target(); }\n",
    )
    .unwrap();

    let indexed = client.request(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"index","arguments":{}}}"#,
    );
    assert!(indexed.contains("changed=6"), "{indexed}");
    let generation = database_generation(&fixture.path.join(".git/graphr/index.db"));
    let changed = client.request(
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"changes","arguments":{"depth":6,"max_nodes":50}}}"#,
    );
    let text = response_text(&changed);
    for expected in [
        "changed supported src/lib.rs",
        "deleted supported src/removed.rs",
        "renamed supported src/moved.rs -> src/renamed.rs",
        "untracked supported src/untracked.rs",
        "target",
        "helper",
        "moved_symbol",
        "first_untracked",
        "second_untracked",
        "checks_untracked",
        "risk overall=",
        "flow ",
        "test <-",
        "caller <-",
    ] {
        assert!(text.contains(expected), "missing {expected}: {changed}");
    }
    assert!(text.contains("+    helper();"), "{changed}");
    assert!(text.contains("@@ -0,0 +1,4 @@"), "{changed}");
    assert!(
        text.lines().any(|line| {
            line.starts_with("flow ")
                && line.contains(
                    "first_untracked@src/untracked.rs:1 -> second_untracked@src/untracked.rs:2",
                )
        }),
        "{text}"
    );
    assert!(
        text.lines().any(|line| {
            line.contains("Function first_untracked src/untracked.rs:1")
                && !line.contains("direct-test-gap")
        }),
        "{text}"
    );
    assert!(
        text.contains("Test checks_untracked src/untracked.rs:3"),
        "{text}"
    );
    assert!(text.contains("-pub fn removed_symbol() {}"), "{changed}");
    assert!(
        text.contains("diff --git a/src/changed.rs b/src/changed.rs"),
        "{changed}"
    );
    assert!(
        text.contains("diff --git a/src/untracked.rs b/src/untracked.rs"),
        "{changed}"
    );
    assert!(
        text.contains("+pub fn first_untracked() { second_untracked(); }"),
        "{changed}"
    );
    let graph = text.split_once("graph\n").unwrap().1;
    assert!(graph.contains("unmapped src/lib.rs"), "{changed}");
    assert!(!graph.contains("removed_symbol"), "{changed}");
    assert!(!graph.contains("ignored_symbol"), "{changed}");
    assert!(text.len() <= 8192, "{}", text.len());
    assert!(text.contains(&format!(":{generation}:")), "{changed}");
    assert_eq!(
        database_generation(&fixture.path.join(".git/graphr/index.db")),
        generation
    );
    let repeated = client.request(
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"changes","arguments":{"depth":6,"max_nodes":50}}}"#,
    );
    assert_eq!(response_text(&repeated), text);

    for invalid in [
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"changes","arguments":{"base":"-HEAD"}}}"#,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"changes","arguments":{"base":"missing"}}}"#,
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"changes","arguments":{"depth":7}}}"#,
        r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"changes","arguments":{"max_nodes":0}}}"#,
        r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"changes","arguments":{"base":"HEAD..HEAD"}}}"#,
    ] {
        let response = client.request(invalid);
        assert!(tool_failed(&response), "{response}");
        assert!(response.len() <= 8192, "{response}");
    }
    let bounded = client.request(
        r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"changes","arguments":{"depth":0,"max_nodes":1}}}"#,
    );
    assert!(bounded.contains("changed_symbols_omitted=5"), "{bounded}");
    assert!(bounded.contains("neighborhood_omitted=true"), "{bounded}");
    assert!(!bounded.contains("[truncated]"), "{bounded}");
    assert!(bounded.len() <= 8192, "{bounded}");
    client.close();
}

#[test]
fn changes_collapses_cargo_vendor_by_default_and_keeps_full_mode() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(fixture.path.join("src/lib.rs"), "pub fn stable() {}\n").unwrap();
    let sha2 = fixture.path.join(".cargo/vendor/sha2");
    fs::create_dir_all(sha2.join("src")).unwrap();
    fs::write(
        sha2.join("Cargo.toml"),
        "[package]\nname = \"sha2\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(sha2.join(".cargo-checksum.json"), "{\"old\":true}\n").unwrap();
    fs::write(sha2.join("src/lib.rs"), "pub fn old_digest() {}\n").unwrap();
    fs::write(
        fixture.path.join(".cargo/vendor/build.rs"),
        "pub fn old_vendor_root() {}\n",
    )
    .unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git(
        &fixture.path,
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
        fixture.path.join("src/canonical.rs"),
        "pub fn canonical_digest() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join(".cargo/vendor/build.rs"),
        "pub fn vendor_root() {}\n",
    )
    .unwrap();
    fs::write(sha2.join(".cargo-checksum.json"), "{\"new\":true}\n").unwrap();
    fs::write(sha2.join("src/lib.rs"), "pub fn vendor_digest() {}\n").unwrap();
    let cpufeatures = fixture.path.join(".cargo/vendor/cpufeatures");
    fs::create_dir_all(cpufeatures.join("src")).unwrap();
    fs::write(
        cpufeatures.join("Cargo.toml"),
        "[package]\nname = \"cpufeatures\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(cpufeatures.join(".cargo-checksum.json"), "{}\n").unwrap();
    fs::write(
        cpufeatures.join("src/lib.rs"),
        "pub fn vendor_feature() {}\n",
    )
    .unwrap();

    let mut client = Client::start(&fixture.path);
    let _ = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
    );
    client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    let boundary = response_text(&client.request(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"changes","arguments":{"depth":1,"max_nodes":10}}}"#,
    ));
    for expected in [
        "dependency_mode=boundary",
        "dependency-boundary root=.cargo/vendor packages=2 files=5 path_digest=",
        "dependency-package name=cpufeatures files=3 supported_sources=1 checksum_files=1",
        "dependency-package name=sha2 files=2 supported_sources=1 checksum_files=1",
        "changed supported .cargo/vendor/build.rs status=modified",
        "untracked supported src/canonical.rs",
        "diff --git a/.cargo/vendor/build.rs b/.cargo/vendor/build.rs",
        "diff --git a/src/canonical.rs b/src/canonical.rs",
        "dependency_analysis=collapsed",
    ] {
        assert!(
            boundary.contains(expected),
            "missing {expected}: {boundary}"
        );
    }
    for omitted in [
        ".cargo/vendor/sha2/src/lib.rs",
        "vendor_digest",
        "vendor_feature",
    ] {
        assert!(
            !boundary.contains(omitted),
            "unexpected {omitted}: {boundary}"
        );
    }
    let boundary_graph = boundary.split_once("graph\n").unwrap().1;
    for expected in [
        "Function canonical_digest src/canonical.rs:1",
        "Function vendor_root .cargo/vendor/build.rs:1",
    ] {
        assert!(
            boundary_graph.contains(expected),
            "missing {expected}: {boundary}"
        );
    }

    let full = response_text(&client.request(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"changes","arguments":{"depth":0,"max_nodes":10,"dependency_mode":"full"}}}"#,
    ));
    for expected in [
        "dependency_mode=full",
        "changed supported .cargo/vendor/sha2/src/lib.rs status=modified",
        "diff --git a/.cargo/vendor/sha2/src/lib.rs b/.cargo/vendor/sha2/src/lib.rs",
        "untracked supported .cargo/vendor/cpufeatures/src/lib.rs",
        "diff --git a/.cargo/vendor/cpufeatures/src/lib.rs b/.cargo/vendor/cpufeatures/src/lib.rs",
        "dependency_analysis=full",
    ] {
        assert!(full.contains(expected), "missing {expected}: {full}");
    }
    assert!(!full.contains("dependency-package name="), "{full}");
    let full_graph = full.split_once("graph\n").unwrap().1;
    for expected in [
        "Function vendor_digest .cargo/vendor/sha2/src/lib.rs:1",
        "Function vendor_feature .cargo/vendor/cpufeatures/src/lib.rs:1",
    ] {
        assert!(full_graph.contains(expected), "missing {expected}: {full}");
    }
    client.close();
}

#[test]
fn changes_pages_complete_inventory_diff_and_flows() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::create_dir_all(fixture.path.join("tests/fixtures")).unwrap();
    fs::write(
        fixture.path.join(".gitignore"),
        "tests/fixtures/ignored.txt\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        review_fixture_source(false),
    )
    .unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git(
        &fixture.path,
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
    index_repository(&fixture.path, true);

    fs::write(fixture.path.join("src/lib.rs"), review_fixture_source(true)).unwrap();
    fs::write(
        fixture.path.join("tests/fixtures/alias-registry.v1.tsv"),
        "one\ntwo\nthree\n",
    )
    .unwrap();
    fs::write(fixture.path.join("tests/fixtures/ignored.txt"), "ignored\n").unwrap();
    index_repository(&fixture.path, false);

    let mut client = Client::start(&fixture.path);
    let _ = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
    );
    client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let initial = response_text(&client.request(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"changes","arguments":{"depth":6,"max_nodes":50}}}"#,
    ));
    for expected in [
        "changed supported src/lib.rs status=modified additions=14 deletions=14",
        "untracked unsupported tests/fixtures/alias-registry.v1.tsv additions=3 deletions=0",
        "total_hunks=14",
        "all_path_hunks=15",
        "all_path_additions=17 all_path_deletions=14",
        "changed_symbols_total=14",
        "flows_total=3",
        "review_complete=false",
        "review_complete_when_pages_exhausted=false",
    ] {
        assert!(initial.contains(expected), "missing {expected}: {initial}");
    }
    assert!(!initial.contains("ignored.txt"), "{initial}");
    assert!(!initial.contains("[truncated]"), "{initial}");
    assert!(initial.len() <= 8192, "{}", initial.len());
    let diff_totals = assert_page_accounting(
        &initial,
        "diff",
        [
            "emitted_hunks",
            "partial_hunks",
            "total_hunks",
            "prior_hunks",
            "remaining_hunks",
        ],
        "diff_next_cursor",
    );
    let graph_totals = assert_page_accounting(
        &initial,
        "graph",
        [
            "emitted_flows",
            "partial_flows",
            "discovered_flows",
            "prior_flows",
            "remaining_discovered_flows",
        ],
        "graph_next_cursor",
    );
    assert_eq!(page_metric(&initial, "graph", "total_flows"), 3);

    let first_diff_cursor = page_cursor(&initial, "diff_next_cursor").unwrap();
    let repeated_a = changes_page(&mut client, 3, &first_diff_cursor);
    let repeated_b = changes_page(&mut client, 4, &first_diff_cursor);
    assert_eq!(repeated_a, repeated_b);
    assert!(repeated_a.starts_with("diff\n"), "{repeated_a}");
    assert!(!repeated_a.contains("\ngraph\n"), "{repeated_a}");
    assert_eq!(
        assert_page_accounting(
            &repeated_a,
            "diff",
            [
                "emitted_hunks",
                "partial_hunks",
                "total_hunks",
                "prior_hunks",
                "remaining_hunks",
            ],
            "diff_next_cursor",
        ),
        diff_totals
    );

    let mut next_id = 5;
    let mut diff_pages = initial.clone();
    let mut cursor = Some(first_diff_cursor.clone());
    while let Some(token) = cursor {
        let page = changes_page(&mut client, next_id, &token);
        next_id += 1;
        assert!(page.len() <= 8192, "{}", page.len());
        assert!(!page.contains("[truncated]"), "{page}");
        assert_eq!(
            assert_page_accounting(
                &page,
                "diff",
                [
                    "emitted_hunks",
                    "partial_hunks",
                    "total_hunks",
                    "prior_hunks",
                    "remaining_hunks",
                ],
                "diff_next_cursor",
            ),
            diff_totals
        );
        cursor = page_cursor(&page, "diff_next_cursor");
        diff_pages.push_str(&page);
    }
    assert!(diff_pages.contains("LAST_PAGE_SENTINEL"), "{diff_pages}");
    assert_eq!(
        diff_pages
            .lines()
            .filter(|line| line.starts_with("@@ "))
            .count(),
        14,
        "{diff_pages}"
    );

    let mut graph_pages = initial.clone();
    let mut cursor = page_cursor(&initial, "graph_next_cursor");
    assert!(
        cursor.is_some(),
        "graph unexpectedly fit on one page: {initial}"
    );
    while let Some(token) = cursor {
        let page = changes_page(&mut client, next_id, &token);
        next_id += 1;
        assert!(page.len() <= 8192, "{}", page.len());
        assert!(!page.contains("[truncated]"), "{page}");
        assert!(page.starts_with("graph\n"), "{page}");
        assert!(!page.contains("\ndiff\n"), "{page}");
        assert_eq!(
            assert_page_accounting(
                &page,
                "graph",
                [
                    "emitted_flows",
                    "partial_flows",
                    "discovered_flows",
                    "prior_flows",
                    "remaining_discovered_flows",
                ],
                "graph_next_cursor",
            ),
            graph_totals
        );
        assert_eq!(page_metric(&page, "graph", "total_flows"), 3);
        cursor = page_cursor(&page, "graph_next_cursor");
        graph_pages.push_str(&page);
    }
    assert_eq!(
        graph_pages
            .lines()
            .filter(|line| line.starts_with("flow "))
            .count(),
        3,
        "{graph_pages}"
    );

    fs::write(
        fixture.path.join("src/lib.rs"),
        format!("{}// cursor is now stale\n", review_fixture_source(true)),
    )
    .unwrap();
    let cached = changes_page(&mut client, next_id, &first_diff_cursor);
    next_id += 1;
    assert_eq!(cached, repeated_a, "cursor did not retain its snapshot");
    let refreshed = response_text(&client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":{next_id},"method":"tools/call","params":{{"name":"changes","arguments":{{"depth":6,"max_nodes":50}}}}}}"#,
    )));
    next_id += 1;
    assert!(
        refreshed.contains("diff_next_cursor="),
        "refreshed diff unexpectedly fit one page: {refreshed}"
    );
    let stale = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":{next_id},"method":"tools/call","params":{{"name":"changes","arguments":{{"depth":6,"max_nodes":50,"cursor":"{first_diff_cursor}"}}}}}}"#,
    ));
    assert!(tool_failed(&stale), "{stale}");
    assert!(stale.contains("stale changes cursor"), "{stale}");
    client.close();
}

#[test]
fn changes_reports_only_residual_lines_in_a_mixed_rust_hunk() {
    const EDITED: &str = "use std::fmt::Debug;\nconst FLAG: bool = true;\nmacro_rules! identity { ($value:expr) => { $value }; }\n// syntax glue\npub fn first() { let _: bool = identity!(FLAG); }\n\npub fn second() { let _ = std::any::type_name::<dyn Debug>(); }\n";

    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git(
        &fixture.path,
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
    index_repository(&fixture.path, true);
    fs::write(fixture.path.join("src/lib.rs"), EDITED).unwrap();
    index_repository(&fixture.path, false);

    let mut client = Client::start(&fixture.path);
    let _ = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
    );
    client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let changes = response_text(&client.request(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"changes","arguments":{"depth":0,"max_nodes":50}}}"#,
    ));

    for expected in [
        "changed_symbols_total=2",
        " Function first src/lib.rs:5",
        " Function second src/lib.rs:7",
        "unmapped src/lib.rs:1-4,6",
    ] {
        assert!(changes.contains(expected), "missing {expected}: {changes}");
    }
    assert!(!changes.contains("unmapped src/lib.rs:1-7"), "{changes}");
    client.close();
}

fn review_fixture_source(edited: bool) -> String {
    let mut source = String::from(
        "pub static ALIASES: &[u8] = include_bytes!(\"../tests/fixtures/alias-registry.v1.tsv\");\n\n",
    );
    for index in 0..14 {
        let name = format!("changed_{index}_{}", "long_identifier_segment_".repeat(5));
        if index < 3 {
            let entry = format!("entry_{index}_{}", "long_identifier_segment_".repeat(5));
            source.push_str(&format!("pub fn {entry}() {{ {name}(); }}\n\n"));
        }
        let value = if edited {
            format!(
                "{}{}",
                "é".repeat(400),
                if index == 13 {
                    "LAST_PAGE_SENTINEL"
                } else {
                    ""
                }
            )
        } else {
            "x".repeat(800)
        };
        source.push_str(&format!("pub fn {name}() {{ let _ = \"{value}\"; }}\n"));
        source.push_str("// unchanged separator one\n// unchanged separator two\n\n");
    }
    source
}

fn page_cursor(output: &str, label: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.strip_prefix(label)
            .and_then(|value| value.strip_prefix('='))
            .map(str::to_owned)
    })
}

fn changes_page(client: &mut Client, id: u32, cursor: &str) -> String {
    response_text(&client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"changes","arguments":{{"depth":6,"max_nodes":50,"cursor":"{cursor}"}}}}}}"#,
    )))
}

fn assert_page_accounting(
    output: &str,
    section: &str,
    [
        emitted_key,
        partial_key,
        total_key,
        prior_key,
        remaining_key,
    ]: [&str; 5],
    cursor_label: &str,
) -> (usize, usize) {
    let emitted = page_metric(output, section, "emitted_bytes");
    let total = page_metric(output, section, "total_bytes");
    let prior = page_metric(output, section, "prior_bytes");
    let remaining = page_metric(output, section, "remaining_bytes");
    assert_eq!(prior + emitted + remaining, total, "{output}");

    let line = page_metadata_line(output, section);
    let (start, end) = page_field(line, "byte_range").split_once("..").unwrap();
    assert_eq!(start.parse::<usize>().unwrap(), prior, "{output}");
    assert_eq!(end.parse::<usize>().unwrap(), prior + emitted, "{output}");

    let emitted_records = page_metric(output, section, emitted_key);
    let partial_records = page_metric(output, section, partial_key);
    let total_records = page_metric(output, section, total_key);
    let prior_records = page_metric(output, section, prior_key);
    let remaining_records = page_metric(output, section, remaining_key);
    assert_eq!(
        prior_records + emitted_records + partial_records + remaining_records,
        total_records,
        "{output}"
    );
    assert_eq!(
        page_field(line, "page_complete") == "true",
        page_cursor(output, cursor_label).is_none(),
        "{output}"
    );
    (total, total_records)
}

fn page_metric(output: &str, section: &str, key: &str) -> usize {
    page_field(page_metadata_line(output, section), key)
        .parse()
        .unwrap()
}

fn page_metadata_line<'a>(output: &'a str, section: &str) -> &'a str {
    output
        .lines()
        .find(|line| {
            line.split_ascii_whitespace().next() == Some(section) && line.contains("emitted_bytes=")
        })
        .unwrap()
}

fn page_field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split_ascii_whitespace()
        .find_map(|field| {
            let (name, value) = field.split_once('=')?;
            (name == key).then_some(value)
        })
        .unwrap()
}

#[test]
fn concurrent_initial_indexes_serialize() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/lib.rs"), "pub fn run() {}\n").unwrap();
    init_git(&fixture.path);

    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphr"));
        command
            .args(["index", fixture.path.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().unwrap()
    };
    let first = command();
    let second = command();
    let mut outputs = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    for output in &outputs {
        assert!(output.status.success(), "{:?}", output.stderr);
    }
    outputs.sort_by(|left, right| left.stdout.cmp(&right.stdout));
    assert_eq!(
        String::from_utf8(outputs[0].stdout.clone()).unwrap(),
        "indexed generation=1 changed=0 skipped=0\n"
    );
    assert_eq!(
        String::from_utf8(outputs[1].stdout.clone()).unwrap(),
        "indexed generation=1 changed=1 skipped=0\n"
    );

    let database = fixture.path.join(".git/graphr/index.db");
    let concurrent = normalized_graph(&database);
    let rebuilt = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args(["index", fixture.path.to_str().unwrap(), "--rebuild"])
        .output()
        .unwrap();
    assert!(rebuilt.status.success(), "{:?}", rebuilt.stderr);
    assert_eq!(normalized_graph(&database), concurrent);
    assert_eq!(
        Connection::open(database)
            .unwrap()
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn rust_index_search_view_over_mcp() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/mailer.rs"), "pub struct Mailer;\n").unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "mod mailer;\nuse crate::mailer::Mailer;\nimpl Mailer { pub fn dispatch() {} }\npub fn register() { Mailer::dispatch(); }\n#[test]\nfn register_dispatches() { register(); }\n",
    )
    .unwrap();
    fs::write(fixture.path.join("src/bad.rs"), [0xff]).unwrap();
    fs::File::create(fixture.path.join("src/too_big.rs"))
        .unwrap()
        .set_len(2 * 1024 * 1024 + 1)
        .unwrap();
    fs::write(fixture.path.join("src/real.txt"), "fn hidden() {}\n").unwrap();
    symlink("real.txt", fixture.path.join("src/link.rs")).unwrap();
    let fifo = fixture.path.join("src/pipe.rs");
    fs::write(&fifo, "fn replaced_by_fifo() {}\n").unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "src/pipe.rs"]);
    fs::remove_file(&fifo).unwrap();
    let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let indexed = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args(["index", fixture.path.to_str().unwrap(), "--rebuild"])
        .env("GIT_LITERAL_PATHSPECS", "1")
        .output()
        .unwrap();
    assert!(indexed.status.success(), "{:?}", indexed.stderr);
    assert_eq!(
        String::from_utf8(indexed.stdout).unwrap(),
        "indexed generation=1 changed=2 skipped=4\n"
    );

    let database = fixture.path.join(".git/graphr/index.db");
    let connection = Connection::open(&database).unwrap();
    let sqlite: String = connection
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .unwrap();
    assert!(version_at_least(&sqlite, [3, 51, 3]));
    let node_count: i64 = connection
        .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
        .unwrap();
    connection.pragma_update(None, "user_version", 999).unwrap();
    drop(connection);

    let rejected = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args(["index", fixture.path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let connection = Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM nodes", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        node_count
    );
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        999
    );
    drop(connection);

    let rebuilt = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args(["index", fixture.path.to_str().unwrap(), "--rebuild"])
        .output()
        .unwrap();
    assert!(rebuilt.status.success(), "{:?}", rebuilt.stderr);
    assert_eq!(
        String::from_utf8(rebuilt.stdout).unwrap(),
        "indexed generation=2 changed=2 skipped=4\n"
    );

    let connection = Connection::open(&database).unwrap();
    connection.execute("DROP INDEX nodes_parent", []).unwrap();
    drop(connection);
    let healed = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args(["index", fixture.path.to_str().unwrap(), "--rebuild"])
        .output()
        .unwrap();
    assert!(healed.status.success(), "{:?}", healed.stderr);
    assert_eq!(
        String::from_utf8(healed.stdout).unwrap(),
        "indexed generation=3 changed=2 skipped=4\n"
    );
    let connection = Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='nodes_parent'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT on_delete FROM pragma_foreign_key_list('nodes') WHERE \"from\"='parent_id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "SET NULL"
    );
    drop(connection);

    let mut invalid_client = Client::start(&fixture.path);
    invalid_client.notify(r#"{"jsonrpc":"2.0","id":0,"method":"tools/list","params":{}}"#);
    match invalid_client.lines.recv_timeout(Duration::from_secs(5)) {
        Ok(response) => assert!(response.contains("\"error\""), "{response}"),
        Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => panic!("uninitialized MCP request did not terminate"),
    }
    invalid_client.close();

    let prefix = r#"{"jsonrpc":"2.0","id":8,"method":""#;
    let suffix = r#""}"#;
    let exact_method = "A".repeat(3 * 1024 - prefix.len() - suffix.len());
    let exact_request = format!("{prefix}{exact_method}{suffix}");
    assert_eq!(exact_request.len(), 3 * 1024);
    let mut boundary_client = Client::start(&fixture.path);
    let _ = boundary_client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
    );
    boundary_client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let boundary_response = boundary_client.request(&exact_request);
    assert!(boundary_response.contains("\"error\""));
    assert!(boundary_response.len() <= 4096);
    boundary_client.close();

    let oversized = "A".repeat(10_000);
    for request in [
        format!(r#"{{"jsonrpc":"2.0","id":8,"method":"{oversized}"}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":"{oversized}","method":"ping"}}"#),
    ] {
        let mut bounded_client = Client::start(&fixture.path);
        let _ = bounded_client.request(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
        );
        bounded_client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        let _ = bounded_client.try_notify(&request);
        match bounded_client.lines.recv_timeout(Duration::from_secs(5)) {
            Ok(response) => assert!(response.len() <= 4096, "oversized response"),
            Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => panic!("oversized MCP request did not terminate"),
        }
        bounded_client.close();
    }

    let real_git = find_executable("git");
    let wrapper_dir = fixture.path.join("git-wrapper");
    let wrapper = wrapper_dir.join("git");
    let block_git = fixture.path.join("block-git");
    let git_entered = fixture.path.join("git-entered");
    let git_finished = fixture.path.join("git-finished");
    fs::create_dir(&wrapper_dir).unwrap();
    fs::write(
        &wrapper,
        "#!/bin/sh\nif [ -e \"$GRAPHR_BLOCK_GIT\" ]; then\n  printf '%s\\n' \"$$\" > \"$GRAPHR_GIT_ENTERED\"\n  exec sleep 600\nfi\n\"$GRAPHR_REAL_GIT\" \"$@\"\nstatus=$?\nprintf 'done\\n' >> \"$GRAPHR_GIT_FINISHED\"\nexit \"$status\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let wrapper_path = env::join_paths(
        std::iter::once(wrapper_dir.clone())
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let configure_git = |command: &mut Command| {
        command
            .env("PATH", &wrapper_path)
            .env("GRAPHR_REAL_GIT", &real_git)
            .env("GRAPHR_BLOCK_GIT", &block_git)
            .env("GRAPHR_GIT_ENTERED", &git_entered)
            .env("GRAPHR_GIT_FINISHED", &git_finished);
    };

    let generation_before_startup_eof = database_generation(&database);
    fs::write(&block_git, []).unwrap();
    let startup_client = Client::start_with(&fixture.path, configure_git);
    let startup_git = wait_for_pid(&git_entered);
    startup_client.close();
    wait_for_exit(startup_git);
    assert_eq!(
        database_generation(&database),
        generation_before_startup_eof
    );
    fs::remove_file(&block_git).unwrap();
    fs::remove_file(&git_entered).unwrap();

    let lock = Connection::open(&database).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let generation_before_locked_startup = database_generation(&database);
    let _ = fs::remove_file(&git_finished);
    let mut locked_startup = Client::start_with(&fixture.path, configure_git);
    wait_for_git_calls(&git_finished, 3);
    thread::sleep(Duration::from_millis(50));
    assert!(locked_startup.child.try_wait().unwrap().is_none());
    let closed = Instant::now();
    locked_startup.close();
    assert!(closed.elapsed() < Duration::from_secs(1));
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        database_generation(&database),
        generation_before_locked_startup
    );

    let mut client = Client::start_with(&fixture.path, configure_git);
    let initialized = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
    );
    assert!(initialized.contains("\"id\":1"));
    client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    let tools = client.request(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    for name in ["changes", "index", "search", "view"] {
        assert!(tools.contains(&format!("\"name\":\"{name}\"")));
    }
    for invalid in [
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search","arguments":{"query":"dispatch","kind":"method"}}}"#,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"search","arguments":{"query":" ","limit":0}}}"#,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"view","arguments":{"node_ref":"bad","depth":7}}}"#,
    ] {
        let response = client.request(invalid);
        assert!(tool_failed(&response), "{response}");
    }
    let huge = "A".repeat(1_000);
    for request in [
        format!(
            r#"{{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{{"name":"search","arguments":{{"query":"dispatch","kind":"{huge}"}}}}}}"#
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"name":"view","arguments":{{"node_ref":"bad","depth":"{huge}"}}}}}}"#
        ),
    ] {
        let response = client.request(&request);
        assert!(tool_failed(&response), "{response}");
        assert!(
            response.len() < 512,
            "oversized MCP error: {}",
            response.len()
        );
    }

    let generation_before_cancel = database_generation(&database);
    fs::write(&block_git, []).unwrap();
    client.notify(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"index","arguments":{}}}"#,
    );
    let blocked_git = wait_for_pid(&git_entered);
    client.notify(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3,"reason":"test"}}"#,
    );
    wait_for_exit(blocked_git);
    fs::remove_file(&block_git).unwrap();
    let pong = client.request(r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#);
    assert!(pong.contains("\"id\":4"), "{pong}");

    let expected_generation = generation_before_cancel;
    let mut reindexed = String::new();
    for id in 10..30 {
        reindexed = client.request(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"index","arguments":{{}}}}}}"#
        ));
        if !reindexed.contains("index busy") {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        reindexed.contains(&format!(
            "indexed generation={expected_generation} changed=0 skipped=4"
        )),
        "{reindexed}"
    );
    assert_eq!(database_generation(&database), expected_generation);

    let search = client.request(
        r#"{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"search","arguments":{"query":"dispatch"}}}"#,
    );
    assert!(search.contains("dispatch"), "{search}");
    let search_text = response_text(&search);
    let node_ref = search_text.split_whitespace().next().unwrap().to_owned();

    let view = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":41,"method":"tools/call","params":{{"name":"view","arguments":{{"node_ref":"{node_ref}","depth":6,"max_nodes":30}}}}}}"#
    ));
    assert!(view.contains("dispatch"), "{view}");
    assert!(view.contains("register"), "{view}");
    assert!(view.contains("Mailer"), "{view}");
    assert!(view.contains("Mailer src/mailer.rs:1"), "{view}");

    for (id, query, kind, member) in [
        (50, "Mailer", "type", "dispatch"),
        (52, "lib", "file", "register_dispatches"),
    ] {
        let search = client.request(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"search","arguments":{{"query":"{query}","kind":"{kind}"}}}}}}"#
        ));
        let search_text = response_text(&search);
        let node_ref = search_text.split_whitespace().next().unwrap();
        let view = client.request(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{"name":"view","arguments":{{"node_ref":"{node_ref}","depth":1}}}}}}"#,
            id + 1
        ));
        assert!(view.contains(member), "{view}");
        assert!(view.contains("member ->"), "{view}");
    }

    let no_op = client.request(
        r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"index","arguments":{}}}"#,
    );
    assert!(
        no_op.contains(&format!(
            "indexed generation={expected_generation} changed=0 skipped=4"
        )),
        "{no_op}"
    );
    let still_valid = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":43,"method":"tools/call","params":{{"name":"view","arguments":{{"node_ref":"{node_ref}"}}}}}}"#
    ));
    assert!(still_valid.contains("dispatch"), "{still_valid}");

    fs::write(
        fixture.path.join("src/mailer.rs"),
        "pub struct Mailer;\npub fn added() {}\n",
    )
    .unwrap();
    let changed = client.request(
        r#"{"jsonrpc":"2.0","id":44,"method":"tools/call","params":{"name":"index","arguments":{}}}"#,
    );
    assert!(
        changed.contains(&format!(
            "indexed generation={} changed=1 skipped=4",
            expected_generation + 1
        )),
        "{changed}"
    );
    let stale = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":45,"method":"tools/call","params":{{"name":"view","arguments":{{"node_ref":"{node_ref}"}}}}}}"#
    ));
    assert!(stale.contains("stale node_ref"), "{stale}");

    let lock = Connection::open(&database).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let generation_before_eof = database_generation(&database);
    client.notify(
        r#"{"jsonrpc":"2.0","id":46,"method":"tools/call","params":{"name":"index","arguments":{}}}"#,
    );
    let busy = client.request(
        r#"{"jsonrpc":"2.0","id":47,"method":"tools/call","params":{"name":"changes","arguments":{}}}"#,
    );
    assert!(busy.contains("changes busy"), "{busy}");
    thread::sleep(Duration::from_millis(50));
    let closed = Instant::now();
    client.close();
    assert!(closed.elapsed() < Duration::from_secs(1));
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(database_generation(&database), generation_before_eof);
}

fn index_repository(path: &Path, rebuild: bool) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphr"));
    command.arg("index").arg(path);
    if rebuild {
        command.arg("--rebuild");
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    String::from_utf8(output.stdout).unwrap()
}

fn assert_incremental_matches_rebuild(incremental: &Path, oracle: &Path, expected_changed: usize) {
    let indexed = index_repository(incremental, false);
    assert!(
        indexed.contains(&format!("changed={expected_changed} skipped=0")),
        "{indexed}"
    );
    index_repository(oracle, true);
    assert_eq!(semantic_graph(incremental), semantic_graph(oracle));
}

fn named_edge_count(path: &Path, source: &str, target: &str) -> i64 {
    Connection::open(path.join(".git/graphr/index.db"))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM edges edge
               JOIN nodes source ON source.id=edge.source_id
               JOIN nodes target ON target.id=edge.target_id
              WHERE source.name=?1 AND target.name=?2",
            [source, target],
            |row| row.get(0),
        )
        .unwrap()
}

fn trait_implementation_count(path: &Path, implementor: &str, trait_: &str) -> i64 {
    Connection::open(path.join(".git/graphr/index.db"))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM trait_implementations implementation
               JOIN nodes implementor ON implementor.id=implementation.resolved_implementor_id
               JOIN nodes trait ON trait.id=implementation.resolved_trait_id
              WHERE implementor.name=?1 AND trait.name=?2",
            [implementor, trait_],
            |row| row.get(0),
        )
        .unwrap()
}

fn assert_resolution(path: &Path, support: Option<i64>, parent_kind: &str) {
    let connection = Connection::open(path.join(".git/graphr/index.db")).unwrap();
    let actual = connection
        .query_row(
            "SELECT e.support_count FROM edges e
               JOIN nodes source ON source.id=e.source_id
               JOIN nodes target ON target.id=e.target_id
              WHERE source.name='call' AND target.name='answer' AND e.kind='CALLS'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(actual, support);
    assert_eq!(
        connection
            .query_row(
                "SELECT parent.kind FROM nodes method
                   JOIN nodes parent ON parent.id=method.parent_id
                  WHERE method.name='ping'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        parent_kind
    );
}

fn semantic_graph(path: &Path) -> Vec<String> {
    let connection = Connection::open(path.join(".git/graphr/index.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT value FROM (
                 SELECT 'file:' || json_array(
                            path, language, git_oid, hex(content_hash), parse_context, byte_size
                        ) AS value
                   FROM files
                 UNION ALL
                 SELECT 'node:' || json_array(
                            file.path, node.kind, node.name, node.qualified_name,
                            parent.qualified_name, node.owner_key, node.line_start,
                            node.line_end, node.signature
                        )
                   FROM nodes node
                   JOIN files file ON file.id=node.file_id
                   LEFT JOIN nodes parent ON parent.id=node.parent_id
                 UNION ALL
                 SELECT 'key:' || json_array(node.qualified_name, node_key.key)
                   FROM node_keys node_key JOIN nodes node ON node.id=node_key.node_id
                 UNION ALL
                 SELECT 'ref:' || json_array(
                            source.qualified_name, reference.kind, reference.line,
                            reference.alias_key, target.qualified_name,
                            ref_key.rank, ref_key.key
                        )
                   FROM refs reference
                   JOIN nodes source ON source.id=reference.source_id
                   LEFT JOIN nodes target ON target.id=reference.resolved_target_id
                   JOIN ref_keys ref_key ON ref_key.ref_id=reference.id
                 UNION ALL
                 SELECT 'impl:' || json_array(
                            file.path, implementation.implementor_key,
                            implementation.trait_key, implementation.line_start,
                            implementation.line_end, implementor.qualified_name,
                            trait.qualified_name
                        )
                   FROM trait_implementations implementation
                   JOIN files file ON file.id=implementation.file_id
                   LEFT JOIN nodes implementor
                     ON implementor.id=implementation.resolved_implementor_id
                   LEFT JOIN nodes trait ON trait.id=implementation.resolved_trait_id
                 UNION ALL
                 SELECT 'edge:' || json_array(
                            source.qualified_name, target.qualified_name,
                            edge.kind, edge.support_count
                        )
                   FROM edges edge
                   JOIN nodes source ON source.id=edge.source_id
                   JOIN nodes target ON target.id=edge.target_id
                 UNION ALL
                 SELECT 'fts:' || json_array(name, qualified_name, path, signature)
                   FROM nodes_fts
             ) ORDER BY value",
        )
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn database_generation(path: &PathBuf) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT generation FROM state WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn normalized_graph(path: &PathBuf) -> Vec<(String, String, String, u32)> {
    let connection = Connection::open(path).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT f.path, n.kind, n.name, n.line_start
               FROM nodes n JOIN files f ON f.id=n.file_id
              ORDER BY f.path, n.qualified_name",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn init_git(path: &Path) {
    git(path, &["init", "--quiet"]);
}

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
}

fn find_executable(name: &str) -> PathBuf {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .and_then(|path| fs::canonicalize(path).ok())
        .unwrap_or_else(|| panic!("cannot find {name}"))
}

fn wait_for_pid(path: &PathBuf) -> i32 {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if let Ok(pid) = fs::read_to_string(path)
            && let Ok(pid) = pid.trim().parse()
        {
            return pid;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("blocked Git did not start");
}

fn wait_for_git_calls(path: &PathBuf, expected: usize) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if fs::read_to_string(path).is_ok_and(|calls| calls.lines().count() >= expected) {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("Git did not finish {expected} calls");
}

fn wait_for_exit(pid: i32) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("cancelled Git process {pid} was not reaped");
}

fn version_at_least(version: &str, floor: [u32; 3]) -> bool {
    version
        .split('.')
        .take(3)
        .map(|part| part.parse().unwrap_or(0))
        .collect::<Vec<u32>>()
        .as_slice()
        >= floor.as_slice()
}

fn response_text(response: &str) -> String {
    rmcp::serde_json::from_str::<rmcp::serde_json::Value>(response).unwrap()["result"]["content"][0]
        ["text"]
        .as_str()
        .expect("text tool result")
        .to_owned()
}

fn tool_failed(response: &str) -> bool {
    response.contains("\"isError\":true")
}

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Client {
    fn start(repository: &PathBuf) -> Self {
        Self::start_with(repository, |_| {})
    }

    fn start_with(repository: &PathBuf, configure: impl FnOnce(&mut Command)) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphr"));
        command
            .arg("serve")
            .arg(repository)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            input: child.stdin.take(),
            child,
            lines,
        }
    }

    fn notify(&mut self, request: &str) {
        self.try_notify(request).unwrap();
    }

    fn try_notify(&mut self, request: &str) -> io::Result<()> {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{request}")?;
        input.flush()
    }

    fn request(&mut self, request: &str) -> String {
        self.notify(request);
        self.lines
            .recv_timeout(Duration::from_secs(5))
            .expect("MCP response")
    }

    fn close(mut self) {
        drop(self.input.take());
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("MCP server did not stop after EOF");
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("graphr-e2e-{}-{unique}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
