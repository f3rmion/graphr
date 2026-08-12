use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, OnceLock};
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

    index_repository(&fixture.path);
    assert_eq!(
        Connection::open(graph_path(&fixture.path))
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
    index_repository(&fixture.path);

    fs::write(fixture.path.join("src/lib.rs"), EDITED).unwrap();
    index_repository(&fixture.path);

    let mut client = Client::start(&fixture.path);
    let changes = client.changes(0, 20, None);
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

    index_repository(&incremental.path);
    index_repository(&oracle.path);
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
    for (query, relation, related) in [
        ("Item", "implements ->", "Marker"),
        ("Marker", "impl <-", "Item"),
    ] {
        let search = client.search(query, Some("type"));
        let search_text = response_text(&search);
        let node_ref = search_text.split_whitespace().next().unwrap();
        let view = client.view(node_ref, 1, 30);
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
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_eq!(
        trait_implementation_count(&incremental.path, "Item", "Marker"),
        1
    );

    for root in [&incremental.path, &oracle.path] {
        fs::write(root.join("src/implementations.rs"), OTHER_IMPL).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
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

    index_repository(&incremental.path);

    let database = graph_path(&incremental.path);
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
    assert_immutable_graphs_match(&incremental.path, &oracle.path);

    for root in [&incremental.path, &oracle.path] {
        fs::write(root.join("src/sample/__init__.py"), EDITED_INIT).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);

    for root in [&incremental.path, &oracle.path] {
        fs::write(
            root.join("src/sample/future.py"),
            "def later():\n    return None\n",
        )
        .unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_eq!(named_edge_count(&incremental.path, "test_run", "later"), 1);

    for root in [&incremental.path, &oracle.path] {
        fs::rename(
            root.join("src/sample/future.py"),
            root.join("src/sample/moved.py"),
        )
        .unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_eq!(named_edge_count(&incremental.path, "test_run", "later"), 0);

    for root in [&incremental.path, &oracle.path] {
        fs::rename(
            root.join("src/sample/moved.py"),
            root.join("src/sample/future.py"),
        )
        .unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_eq!(named_edge_count(&incremental.path, "test_run", "later"), 1);

    for root in [&incremental.path, &oracle.path] {
        fs::remove_file(root.join("src/sample/future.py")).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_eq!(named_edge_count(&incremental.path, "test_run", "later"), 0);

    for root in [&incremental.path, &oracle.path] {
        fs::write(root.join("src/sample/checks.py"), EDITED_CHECKS).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);

    let mut client = Client::start(&incremental.path);
    let search = client.search("Stage", Some("type"));
    let search_text = response_text(&search);
    let node_ref = search_text.split_whitespace().next().unwrap();
    let view = client.view(node_ref, 2, 30);
    assert!(view.contains("member ->"), "{view}");
    assert!(view.contains("dispatch"), "{view}");
    let changes = client.changes(1, 50, None);
    assert!(changes.contains("dispatch"), "{changes}");
    assert!(changes.contains("decorated"), "{changes}");
    client.close();
}

#[test]
fn immutable_snapshots_match_fresh_graphs_through_mutations() {
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

    index_repository(&incremental.path);
    index_repository(&oracle.path);
    assert_eq!(
        semantic_graph(&incremental.path),
        semantic_graph(&oracle.path)
    );
    assert_resolution(&incremental.path, None, "file");

    let generation = database_generation(&graph_path(&incremental.path));
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_eq!(
        database_generation(&graph_path(&incremental.path)),
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
    assert_immutable_graphs_match(&incremental.path, &oracle.path);

    for root in roots {
        fs::remove_file(root.join("crates/app/Cargo.toml")).unwrap();
        fs::remove_file(root.join("crates/app/src/lib.rs")).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);

    for root in roots {
        fs::write(root.join("src/target.rs"), TARGET).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_resolution(&incremental.path, Some(2), "type");

    for root in roots {
        fs::write(root.join("src/caller.rs"), EDITED_CALLER).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_resolution(&incremental.path, Some(3), "type");

    for root in roots {
        fs::create_dir(root.join("src/target")).unwrap();
        fs::write(root.join("src/target/mod.rs"), TARGET).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_resolution(&incremental.path, None, "file");

    for root in roots {
        fs::remove_file(root.join("src/target/mod.rs")).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_resolution(&incremental.path, Some(3), "type");

    for root in roots {
        fs::rename(root.join("src/target.rs"), root.join("src/moved.rs")).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_resolution(&incremental.path, None, "file");

    for root in roots {
        fs::rename(root.join("src/moved.rs"), root.join("src/target.rs")).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
    assert_resolution(&incremental.path, Some(3), "type");

    for root in roots {
        fs::write(root.join("src/caller.rs"), CALLER).unwrap();
    }
    assert_immutable_graphs_match(&incremental.path, &oracle.path);
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
    index_repository(&fixture.path);

    let mut client = Client::start(&fixture.path);
    let clean = client.changes(1, 50, None);
    assert!(
        clean.contains("no changes reason=empty_worktree_delta"),
        "{clean}"
    );

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

    let indexed = client.index_and_wait("boundary");
    assert!(indexed["stats"]["files_parsed"].as_u64().is_some());
    let generation = database_generation(&graph_path(&fixture.path));
    let changed = client.changes(6, 50, None);
    let text = response_text(&changed);
    let mut complete = text.clone();
    let mut graph_cursor = page_cursor(&text, "graph_next_cursor");
    while let Some(token) = graph_cursor {
        let page = changes_page(&mut client, &token);
        assert!(page.len() <= 8192, "{}", page.len());
        graph_cursor = page_cursor(&page, "graph_next_cursor");
        complete.push_str(&page);
    }
    for expected in [
        "changed source rust src/lib.rs",
        "deleted source rust src/removed.rs",
        "renamed source rust src/moved.rs -> src/renamed.rs",
        "untracked source rust src/untracked.rs",
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
        assert!(
            complete.contains(expected),
            "missing {expected}: {complete}"
        );
    }
    assert!(text.contains("+    helper();"), "{changed}");
    assert!(text.contains("@@ -0,0 +1,4 @@"), "{changed}");
    assert!(
        complete.lines().any(|line| {
            line.starts_with("flow ")
                && line.contains(
                    "first_untracked@src/untracked.rs:1 -> second_untracked@src/untracked.rs:2",
                )
        }),
        "{complete}"
    );
    assert!(
        complete.lines().any(|line| {
            line.contains("Function first_untracked src/untracked.rs:1")
                && !line.contains("no-static-test-path")
        }),
        "{complete}"
    );
    assert!(
        complete.lines().any(|line| {
            line.contains("Function second_untracked src/untracked.rs:2")
                && line.contains("indirect-test-covered")
                && !line.contains("no-static-test-path")
        }),
        "{complete}"
    );
    assert!(
        complete.contains("Test checks_untracked src/untracked.rs:3"),
        "{complete}"
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
    assert!(graph.contains("file-mapped src/lib.rs"), "{changed}");
    assert!(graph.contains("unmapped_ranges=0"), "{changed}");
    assert!(!graph.contains("removed_symbol"), "{changed}");
    assert!(!graph.contains("ignored_symbol"), "{changed}");
    assert!(text.len() <= 8192, "{}", text.len());
    assert!(text.contains(&format!(":{generation}:")), "{changed}");
    assert_eq!(database_generation(&graph_path(&fixture.path)), generation);
    let repeated = client.changes(6, 50, None);
    assert_eq!(response_text(&repeated), text);

    for arguments in [
        rmcp::serde_json::json!({ "snapshot_id": client.snapshot_id(), "base": "HEAD" }),
        rmcp::serde_json::json!({ "snapshot_id": client.snapshot_id(), "depth": 7 }),
        rmcp::serde_json::json!({ "snapshot_id": client.snapshot_id(), "max_nodes": 0 }),
    ] {
        let response = client.call("changes", arguments);
        assert!(tool_failed(&response), "{response}");
        assert!(response.len() <= 8192, "{response}");
    }
    let bounded = client.changes(0, 1, None);
    assert!(bounded.contains("changed_symbols_omitted=0"), "{bounded}");
    assert!(bounded.contains("neighborhood_omitted=false"), "{bounded}");
    assert!(bounded.contains("graph_next_cursor="), "{bounded}");
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
    let boundary = response_text(&client.changes(1, 10, None));
    for expected in [
        "dependency_mode=boundary",
        "dependency-boundary root=.cargo/vendor packages=2 files=5 path_digest=",
        "dependency-package name=cpufeatures files=3 source_files=1 checksum_files=1",
        "dependency-package name=sha2 files=2 source_files=1 checksum_files=1",
        "changed source rust .cargo/vendor/build.rs status=modified",
        "untracked source rust src/canonical.rs",
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

    client.index_and_wait("full");
    let full = response_text(&client.changes(0, 10, None));
    for expected in [
        "dependency_mode=full",
        "changed source rust .cargo/vendor/sha2/src/lib.rs status=modified",
        "diff --git a/.cargo/vendor/sha2/src/lib.rs b/.cargo/vendor/sha2/src/lib.rs",
        "untracked source rust .cargo/vendor/cpufeatures/src/lib.rs",
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
    fs::write(
        fixture.path.join("README.md"),
        "# Review\n\nREQ-1\n\n[Old spec](specs/old.md)\n\n```rust\nfn old() {}\n```\n",
    )
    .unwrap();
    fs::write(fixture.path.join("settings.toml"), "old=true\n").unwrap();
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
    fs::write(fixture.path.join("src/lib.rs"), review_fixture_source(true)).unwrap();
    fs::write(
        fixture.path.join("README.md"),
        "# Review\n\nREQ-2\n\n[New spec](specs/new.md)\n\n```rust\nfn new() {}\n```\n",
    )
    .unwrap();
    fs::write(fixture.path.join("settings.toml"), "old=false\n").unwrap();
    let tsv = format!(
        "id\tvalue\n{}last\tLAST_ARTIFACT_SENTINEL\n",
        (0..48)
            .map(|index| format!("row-{index}\t{}\n", "value".repeat(16)))
            .collect::<String>()
    );
    fs::write(
        fixture.path.join("tests/fixtures/alias-registry.v1.tsv"),
        tsv,
    )
    .unwrap();
    fs::write(fixture.path.join("image.bin"), b"image\0binary\n").unwrap();
    fs::write(fixture.path.join("tests/fixtures/ignored.txt"), "ignored\n").unwrap();
    let mut client = Client::start(&fixture.path);
    let initial = response_text(&client.changes(6, 50, None));
    for expected in [
        "changed source rust src/lib.rs",
        "changed artifact text README.md analyzer=markdown",
        "changed artifact text settings.toml analyzer=generic",
        "untracked artifact text tests/fixtures/alias-registry.v1.tsv analyzer=tsv",
        "untracked artifact omitted image.bin analyzer=generic reason=binary",
        "markdown path=\"README.md\"",
        "artifacts_next_cursor=",
        "review_complete_when_pages_exhausted=false",
        "total_hunks=14",
        "changed_symbols_total=14",
        "flows_total=3",
        "review_complete=false",
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
    let artifact_totals = assert_page_accounting(
        &initial,
        "artifacts",
        [
            "emitted_records",
            "partial_records",
            "total_records",
            "prior_records",
            "remaining_records",
        ],
        "artifacts_next_cursor",
    );
    assert_eq!(page_metric(&initial, "graph", "total_flows"), 3);

    let first_diff_cursor = page_cursor(&initial, "diff_next_cursor").unwrap();
    let repeated_a = changes_page(&mut client, &first_diff_cursor);
    let repeated_b = changes_page(&mut client, &first_diff_cursor);
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

    let mut diff_pages = initial.clone();
    let mut cursor = Some(first_diff_cursor.clone());
    while let Some(token) = cursor {
        let page = changes_page(&mut client, &token);
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

    let mut artifact_pages = initial.clone();
    let mut cursor = page_cursor(&initial, "artifacts_next_cursor");
    while let Some(token) = cursor {
        let page = changes_page(&mut client, &token);
        assert!(page.len() <= 8192, "{}", page.len());
        assert!(page.starts_with("artifacts\n"), "{page}");
        assert!(
            page.contains("review_complete_when_pages_exhausted=false"),
            "{page}"
        );
        assert_eq!(
            assert_page_accounting(
                &page,
                "artifacts",
                [
                    "emitted_records",
                    "partial_records",
                    "total_records",
                    "prior_records",
                    "remaining_records",
                ],
                "artifacts_next_cursor",
            ),
            artifact_totals
        );
        cursor = page_cursor(&page, "artifacts_next_cursor");
        artifact_pages.push_str(&page);
    }
    assert!(artifact_pages.contains("key_basis=first-column"));
    assert!(artifact_pages.contains("LAST_ARTIFACT_SENTINEL"));
    assert!(artifact_pages.contains("diff --git a/README.md b/README.md"));
    assert!(artifact_pages.contains("diff --git a/settings.toml b/settings.toml"));

    let mut graph_pages = initial.clone();
    let mut cursor = page_cursor(&initial, "graph_next_cursor");
    assert!(
        cursor.is_some(),
        "graph unexpectedly fit on one page: {initial}"
    );
    while let Some(token) = cursor {
        let page = changes_page(&mut client, &token);
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
    let cached = changes_page(&mut client, &first_diff_cursor);
    assert_eq!(cached, repeated_a, "cursor did not retain its snapshot");
    client.index_and_wait("boundary");
    let refreshed = response_text(&client.changes(6, 50, None));
    assert!(
        refreshed.contains("diff_next_cursor="),
        "refreshed diff unexpectedly fit one page: {refreshed}"
    );
    let stale = client.changes(6, 50, Some(&first_diff_cursor));
    assert!(tool_failed(&stale), "{stale}");
    assert!(stale.contains("cursor_snapshot_mismatch"), "{stale}");
    client.close();
}

#[test]
fn changes_file_maps_non_symbol_ranges_in_a_mixed_rust_hunk() {
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
    index_repository(&fixture.path);
    fs::write(fixture.path.join("src/lib.rs"), EDITED).unwrap();
    index_repository(&fixture.path);

    let mut client = Client::start(&fixture.path);
    let changes = response_text(&client.changes(0, 50, None));

    for expected in [
        "changed_symbols_total=2",
        " Function first src/lib.rs:5",
        " Function second src/lib.rs:7",
        "file-mapped src/lib.rs:1-4,6",
        "risk_direction=higher-is-riskier",
        "risk_components=flow:",
        "risk_rationale=",
    ] {
        assert!(changes.contains(expected), "missing {expected}: {changes}");
    }
    assert!(changes.contains("mapping_complete=true"), "{changes}");
    assert!(changes.contains("neighborhood_complete=true"), "{changes}");
    assert!(
        changes.contains("review_complete_when_pages_exhausted=true"),
        "{changes}"
    );
    assert!(changes.contains("coverage status=complete"), "{changes}");
    assert!(!changes.contains("file-mapped src/lib.rs:1-7"), "{changes}");
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

fn changes_page(client: &mut Client, cursor: &str) -> String {
    response_text(&client.changes(6, 50, Some(cursor)))
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
fn concurrent_snapshot_publication_is_idempotent() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/lib.rs"), "pub fn run() {}\n").unwrap();
    init_git(&fixture.path);

    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphr"));
        command
            .args([
                "index",
                "--worktree-root",
                fixture.path.to_str().unwrap(),
                "--base",
                "HEAD",
                "--head",
                "HEAD",
                "--target",
                "worktree",
                "--include-untracked",
                "--dependency-mode",
                "boundary",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().unwrap()
    };
    let first = command();
    let second = command();
    let outputs = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    let completions = outputs.map(|output| {
        assert!(output.status.success(), "{:?}", output.stderr);
        rmcp::serde_json::from_slice::<rmcp::serde_json::Value>(output.stdout.trim_ascii()).unwrap()
    });
    assert_eq!(completions[0]["snapshot_id"], completions[1]["snapshot_id"]);
    assert_eq!(
        completions[0]["graph_image_id"],
        completions[1]["graph_image_id"]
    );
    remember_graph(&fixture.path, &completions[0]);
    assert_eq!(
        Connection::open(graph_path(&fixture.path))
            .unwrap()
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn rust_index_search_view_and_inspection_over_explicit_snapshots() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/mailer.rs"), "pub struct Mailer;\n").unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "mod mailer;\nuse crate::mailer::Mailer;\nimpl Mailer { pub fn dispatch() {} }\npub fn register() { Mailer::dispatch(); }\n#[test]\nfn register_dispatches() { register(); }\n",
    )
    .unwrap();
    init_git(&fixture.path);

    let mut client = Client::start(&fixture.path);
    let tools = client.request(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    for name in [
        "inspect_root",
        "index",
        "index_status",
        "cancel_index",
        "search",
        "view",
        "changes",
    ] {
        assert!(tools.contains(&format!("\"name\":\"{name}\"")), "{tools}");
    }

    let inspection = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({
            "worktree_root": fixture.path,
            "snapshot_id": client.snapshot_id(),
        }),
    ));
    assert_eq!(
        inspection["result"]["structuredContent"]["snapshot_matches_worktree"],
        true
    );

    let search = client.search("dispatch", Some("function"));
    assert!(search.contains("dispatch"), "{search}");
    let search_value = response_json(&search);
    assert_eq!(
        search_value["result"]["structuredContent"]["provenance"]["snapshot_id"],
        client.snapshot_id()
    );
    assert!(
        search_value["result"]["structuredContent"]
            .get("text")
            .is_none(),
        "query text was duplicated in structuredContent: {search}"
    );
    let node_ref = response_text(&search)
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let old_snapshot = client.snapshot_id().to_owned();
    let view = client.view(&node_ref, 6, 30);
    for expected in ["dispatch", "register", "Mailer", "Mailer src/mailer.rs:1"] {
        assert!(view.contains(expected), "missing {expected}: {view}");
    }

    fs::write(
        fixture.path.join("src/mailer.rs"),
        "pub struct Mailer;\npub fn added() {}\n",
    )
    .unwrap();
    let divergent = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({
            "worktree_root": fixture.path,
            "snapshot_id": old_snapshot,
        }),
    ));
    assert_eq!(
        divergent["result"]["structuredContent"]["snapshot_matches_worktree"],
        false
    );
    assert!(
        divergent["result"]["structuredContent"]["changed_identity_fields"]
            .as_array()
            .unwrap()
            .contains(&rmcp::serde_json::json!("dirty_digest")),
        "{divergent}"
    );

    client.index_and_wait("boundary");
    assert_ne!(client.snapshot_id(), old_snapshot);
    let mismatch = client.view(&node_ref, 1, 10);
    assert!(tool_failed(&mismatch), "{mismatch}");
    assert!(mismatch.contains("node_snapshot_mismatch"), "{mismatch}");

    let old_view = client.call(
        "view",
        rmcp::serde_json::json!({
            "snapshot_id": old_snapshot,
            "node_ref": node_ref,
            "depth": 1,
            "max_nodes": 10,
        }),
    );
    assert!(old_view.contains("dispatch"), "{old_view}");

    let unknown = client.call(
        "search",
        rmcp::serde_json::json!({
            "snapshot_id": "0".repeat(64),
            "query": "dispatch",
        }),
    );
    assert!(tool_failed(&unknown), "{unknown}");
    assert!(unknown.contains("snapshot_not_found"), "{unknown}");
    client.close();
}

#[test]
fn server_started_with_main_indexes_the_selected_linked_worktree() {
    let worktrees = linked_worktrees("explicit-selection");
    fs::create_dir_all(worktrees.main.join("src")).unwrap();
    fs::write(
        worktrees.main.join("src/main_only.rs"),
        "pub fn main_only_symbol() {}\n",
    )
    .unwrap();
    git(&worktrees.main, &["add", "--", "src/main_only.rs"]);
    git(
        &worktrees.main,
        &[
            "-c",
            "user.name=Graphr Test",
            "-c",
            "user.email=graphr@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "main only",
        ],
    );
    fs::create_dir_all(worktrees.linked.join("src")).unwrap();
    fs::write(
        worktrees.linked.join("src/feature_only.rs"),
        "pub fn feature_only_symbol() {}\n",
    )
    .unwrap();
    git(&worktrees.linked, &["add", "--", "src/feature_only.rs"]);
    git(
        &worktrees.linked,
        &[
            "-c",
            "user.name=Graphr Test",
            "-c",
            "user.email=graphr@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "feature only",
        ],
    );
    let feature_oid = git_output(&worktrees.linked, &["rev-parse", "HEAD"]);
    let feature_git_dir = git_output(
        &worktrees.linked,
        &["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
    );
    let mut client = Client::start_unindexed_with(&worktrees.linked, |command| {
        command
            .current_dir(&worktrees.main)
            .arg("--allow-root")
            .arg(&worktrees.main);
    });
    let main_workspace = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({ "worktree_root": &worktrees.main }),
    ));
    let main_workspace_id =
        main_workspace["result"]["structuredContent"]["identity"]["workspace_id"]
            .as_str()
            .unwrap()
            .to_owned();

    let queued = response_json(&client.call(
        "index",
        rmcp::serde_json::json!({
            "worktree_root": &worktrees.linked,
            "base": "HEAD~1",
            "head": "HEAD",
            "target": { "kind": "commit" },
            "dependency_mode": "boundary",
        }),
    ));
    let job_id = queued["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let completed = client.wait_for_job(&job_id, "completed");
    let completion = &completed["result"]["structuredContent"]["state"]["completion"];
    let snapshot_id = completion["snapshot_id"].as_str().unwrap().to_owned();
    client.snapshot_id = Some(snapshot_id.clone());

    let provenance = &completion["provenance"];
    assert_eq!(
        provenance["worktree_root"],
        fs::canonicalize(&worktrees.linked)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(provenance["git_dir"], feature_git_dir);
    assert_eq!(provenance["branch"], "linked");
    assert_eq!(provenance["head_oid"], feature_oid);
    assert_eq!(provenance["changed_files"], 1);
    assert_eq!(
        provenance["selected_layers"],
        rmcp::serde_json::json!(["committed"])
    );

    let changes = client.changes(1, 50, None);
    let changes_text = response_text(&changes);
    assert!(
        changes_text.contains("added source rust src/feature_only.rs"),
        "{changes}"
    );
    assert!(!changes_text.contains("src/main_only.rs"), "{changes}");
    assert!(
        changes_text.contains("all_path_additions=1 all_path_deletions=0"),
        "{changes}"
    );
    assert!(changes_text.contains("all_path_hunks=1"), "{changes}");
    let search = client.search("feature_only_symbol", Some("function"));
    assert!(
        response_text(&search).contains("feature_only_symbol"),
        "{search}"
    );
    let absent = client.search("main_only_symbol", Some("function"));
    assert!(
        !response_text(&absent).contains("main_only_symbol"),
        "{absent}"
    );
    for response in [
        &queued.to_string(),
        &completed.to_string(),
        &changes,
        &search,
        &absent,
    ] {
        assert!(
            !response.contains(&main_workspace_id),
            "main workspace leaked into selected feature response: {response}"
        );
    }
    assert_eq!(snapshot_id, client.snapshot_id());
    client.close();
}

#[test]
fn unknown_disallowed_replaced_subdirectory_bare_and_symlink_roots_fail_explicitly() {
    let fixture = Fixture::new();
    let repository = fixture.path.join("repository");
    init_git_main(&repository);
    fs::create_dir(repository.join("src")).unwrap();
    let outside = fixture.path.join("outside");
    init_git_main(&outside);
    let bare = fixture.path.join("bare.git");
    git_init_bare(&bare);

    let mut client = Client::start(&repository);
    assert_root_error(
        &client.call(
            "inspect_root",
            rmcp::serde_json::json!({ "worktree_root": fixture.path.join("unknown") }),
        ),
        "root_unknown",
        &[("root", fixture.path.join("unknown").display().to_string())],
    );
    assert_root_error(
        &client.call(
            "inspect_root",
            rmcp::serde_json::json!({ "worktree_root": &outside }),
        ),
        "root_disallowed",
        &[(
            "root",
            fs::canonicalize(&outside).unwrap().display().to_string(),
        )],
    );
    assert_root_error(
        &client.call(
            "inspect_root",
            rmcp::serde_json::json!({ "worktree_root": repository.join("src") }),
        ),
        "root_not_worktree",
        &[("root", repository.join("src").display().to_string())],
    );

    let old_snapshot = client.snapshot_id().to_owned();
    let old_workspace = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({ "worktree_root": &repository }),
    ))["result"]["structuredContent"]["identity"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let moved = fixture.path.join("moved");
    fs::rename(&repository, &moved).unwrap();
    init_git_main(&repository);
    assert_root_error(
        &client.call(
            "inspect_root",
            rmcp::serde_json::json!({
                "worktree_root": &repository,
                "snapshot_id": old_snapshot,
            }),
        ),
        "root_stale",
        &[("root", repository.display().to_string())],
    );
    client.close();

    let mut replacement = Client::start(&repository);
    let replacement_workspace = response_json(&replacement.call(
        "inspect_root",
        rmcp::serde_json::json!({ "worktree_root": &repository }),
    ))["result"]["structuredContent"]["identity"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(replacement_workspace, old_workspace);
    replacement.close();

    let mut bare_client = Client::start_unindexed(&bare);
    assert_root_error(
        &bare_client.call(
            "inspect_root",
            rmcp::serde_json::json!({ "worktree_root": &bare }),
        ),
        "git_metadata_invalid",
        &[],
    );
    bare_client.close();

    let worktrees = linked_worktrees("symlink-root");
    let git_dir = PathBuf::from(git_output(
        &worktrees.linked,
        &["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
    ));
    let escaped = fixture.path.join("escaped-git-dir");
    fs::rename(&git_dir, &escaped).unwrap();
    std::os::unix::fs::symlink(&escaped, &git_dir).unwrap();
    let mut symlink_client = Client::start_unindexed(&worktrees.linked);
    assert_root_error(
        &symlink_client.call(
            "inspect_root",
            rmcp::serde_json::json!({ "worktree_root": &worktrees.linked }),
        ),
        "git_metadata_invalid",
        &[],
    );
    symlink_client.close();
    fs::remove_file(&git_dir).unwrap();
    fs::rename(&escaped, &git_dir).unwrap();
}

#[test]
fn linked_worktrees_report_shared_repository_and_distinct_workspace_identity() {
    let worktrees = linked_worktrees("identity");
    let mut client = Client::start_unindexed_with(&worktrees.main, |command| {
        command.arg("--allow-root").arg(&worktrees.linked);
    });
    let main = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({ "worktree_root": &worktrees.main }),
    ));
    let linked = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({ "worktree_root": &worktrees.linked }),
    ));
    let main_identity = &main["result"]["structuredContent"]["identity"];
    let linked_identity = &linked["result"]["structuredContent"]["identity"];

    assert_eq!(
        main_identity["common_git_dir"],
        linked_identity["common_git_dir"]
    );
    assert_eq!(
        main_identity["repository_id"],
        linked_identity["repository_id"]
    );
    assert_ne!(
        main_identity["workspace_id"],
        linked_identity["workspace_id"]
    );
    assert_ne!(main_identity["git_dir"], linked_identity["git_dir"]);
    assert_ne!(main_identity["index_path"], linked_identity["index_path"]);
    assert_eq!(
        main_identity["worktree_root"],
        fs::canonicalize(&worktrees.main)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(
        linked_identity["worktree_root"],
        fs::canonicalize(&worktrees.linked)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(main_identity["branch"], "main");
    assert_eq!(linked_identity["branch"], "linked");
    assert_eq!(
        main_identity["head_oid"],
        git_output(&worktrees.main, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        linked_identity["head_oid"],
        git_output(&worktrees.linked, &["rev-parse", "HEAD"])
    );
    client.close();
}

#[test]
fn commit_target_reviews_a_branch_without_checking_it_out() {
    let fixture = Fixture::new();
    let repository = fixture.path.join("repository");
    init_git_main(&repository);
    fs::create_dir_all(repository.join("src")).unwrap();
    fs::write(repository.join("src/main.rs"), "pub fn base_symbol() {}\n").unwrap();
    git(&repository, &["add", "--", "."]);
    git_commit(&repository, "base");
    let base_oid = git_output(&repository, &["rev-parse", "HEAD"]);
    git(&repository, &["switch", "--quiet", "-c", "feature"]);
    fs::write(
        repository.join("src/feature.rs"),
        "pub fn unchecked_feature_symbol() {}\n",
    )
    .unwrap();
    git(&repository, &["add", "--", "."]);
    git_commit(&repository, "feature");
    let feature_oid = git_output(&repository, &["rev-parse", "HEAD"]);
    git(&repository, &["switch", "--quiet", "main"]);
    let before = repository_state(&repository);

    let mut client = Client::start_unindexed(&repository);
    let queued = response_json(&client.call(
        "index",
        rmcp::serde_json::json!({
            "worktree_root": &repository,
            "base": "main",
            "head": "feature",
            "target": { "kind": "commit" },
            "dependency_mode": "boundary",
        }),
    ));
    let job_id = queued["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let completed = client.wait_for_job(&job_id, "completed");
    let completion = &completed["result"]["structuredContent"]["state"]["completion"];
    client.snapshot_id = Some(completion["snapshot_id"].as_str().unwrap().to_owned());
    assert_eq!(completion["provenance"]["base_oid"], base_oid);
    assert_eq!(completion["provenance"]["head_oid"], feature_oid);
    assert_eq!(completion["provenance"]["target_state"]["kind"], "commit");
    let search = client.search("unchecked_feature_symbol", Some("function"));
    assert!(
        response_text(&search).contains("unchecked_feature_symbol"),
        "{search}"
    );
    let changes = client.changes(1, 50, None);
    assert!(
        response_text(&changes).contains("src/feature.rs"),
        "{changes}"
    );
    assert_eq!(repository_state(&repository), before);
    client.close();
}

#[test]
fn identical_clean_oids_return_explained_no_changes() {
    let fixture = Fixture::new();
    let repository = fixture.path.join("repository");
    init_git_main(&repository);
    fs::create_dir_all(repository.join("src")).unwrap();
    fs::write(repository.join("src/lib.rs"), "pub fn clean_symbol() {}\n").unwrap();
    git(&repository, &["add", "--", "."]);
    git_commit(&repository, "clean");
    let oid = git_output(&repository, &["rev-parse", "HEAD"]);

    let mut client = Client::start_unindexed(&repository);
    let queued = response_json(&client.call(
        "index",
        rmcp::serde_json::json!({
            "worktree_root": &repository,
            "base": "HEAD",
            "head": "HEAD",
            "target": { "kind": "commit" },
            "dependency_mode": "boundary",
        }),
    ));
    let job_id = queued["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let completed = client.wait_for_job(&job_id, "completed");
    let completion = &completed["result"]["structuredContent"]["state"]["completion"];
    let snapshot_id = completion["snapshot_id"].as_str().unwrap().to_owned();
    client.snapshot_id = Some(snapshot_id.clone());
    let changes = client.changes(1, 50, None);
    let structured = response_json(&changes);
    let provenance = &structured["result"]["structuredContent"]["provenance"];

    assert_eq!(
        response_text(&changes),
        "no changes reason=identical_commit_oids\n"
    );
    assert_eq!(
        structured["result"]["structuredContent"]["no_change_reason"],
        "identical_commit_oids"
    );
    assert_eq!(
        provenance["worktree_root"],
        fs::canonicalize(&repository).unwrap().display().to_string()
    );
    assert_eq!(provenance["base_oid"], oid);
    assert_eq!(provenance["head_oid"], oid);
    assert_eq!(provenance["commits_base_to_head"], 0);
    assert_eq!(provenance["changed_files"], 0);
    assert_eq!(provenance["snapshot_id"], snapshot_id);
    assert_eq!(
        provenance["repository_root"],
        fs::canonicalize(&repository).unwrap().display().to_string()
    );
    assert_eq!(provenance["branch"], "main");
    assert_eq!(provenance["target_state"]["kind"], "commit");
    assert_eq!(provenance["selected_layers"], rmcp::serde_json::json!([]));
    assert_eq!(provenance["dirty_digest"].as_str().unwrap().len(), 64);
    assert_ne!(provenance["repository_id"], rmcp::serde_json::Value::Null);
    assert_ne!(provenance["workspace_id"], rmcp::serde_json::Value::Null);
    assert_ne!(provenance["common_git_dir"], rmcp::serde_json::Value::Null);
    assert_ne!(provenance["git_dir"], rmcp::serde_json::Value::Null);
    client.close();
}

#[test]
fn queued_jobs_ignore_request_cancellation_and_eof_closes_them() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/lib.rs"), "pub fn run() {}\n").unwrap();
    init_git(&fixture.path);

    let wrapper_dir = fixture.path.join("git-wrapper");
    let wrapper = wrapper_dir.join("git");
    let block = fixture.path.join("block-build");
    let entered = fixture.path.join("build-entered");
    fs::create_dir(&wrapper_dir).unwrap();
    fs::write(
        &wrapper,
        "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = rev-list ] && [ -e \"$GRAPHR_BLOCK_BUILD\" ]; then\n    printf '%s\\n' \"$$\" > \"$GRAPHR_BUILD_ENTERED\"\n    exec sleep 600\n  fi\ndone\nexec \"$GRAPHR_REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let wrapper_path = env::join_paths(
        std::iter::once(wrapper_dir)
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let real_git = find_executable("git");
    let configure = |command: &mut Command| {
        command
            .env("PATH", &wrapper_path)
            .env("GRAPHR_REAL_GIT", &real_git)
            .env("GRAPHR_BLOCK_BUILD", &block)
            .env("GRAPHR_BUILD_ENTERED", &entered);
    };

    fs::write(&block, []).unwrap();
    let mut client = Client::start_unindexed_with(&fixture.path, configure);
    let request_id = client.next_id;
    let job_id = client.queue_index("boundary");
    wait_for_file(&entered);
    client.notify(
        &rmcp::serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": request_id, "reason": "request finished" },
        })
        .to_string(),
    );
    let status = response_json(&client.call(
        "index_status",
        rmcp::serde_json::json!({ "job_id": &job_id }),
    ));
    assert_ne!(
        status["result"]["structuredContent"]["state"]["state"], "cancelled",
        "request cancellation cancelled the queued job: {status}"
    );
    client.call(
        "cancel_index",
        rmcp::serde_json::json!({ "job_id": &job_id }),
    );
    client.wait_for_job(&job_id, "cancelled");

    fs::remove_file(&entered).unwrap();
    let second_job = client.queue_index("boundary");
    wait_for_file(&entered);
    assert!(!second_job.is_empty());
    client.close();
}

fn index_repository(path: &Path) -> rmcp::serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args([
            "index",
            "--worktree-root",
            path.to_str().unwrap(),
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--target",
            "worktree",
            "--include-untracked",
            "--dependency-mode",
            "boundary",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let completion: rmcp::serde_json::Value =
        rmcp::serde_json::from_slice(output.stdout.trim_ascii()).unwrap();
    remember_graph(path, &completion);
    completion
}

fn assert_immutable_graphs_match(incremental: &Path, oracle: &Path) {
    index_repository(incremental);
    index_repository(oracle);
    assert_eq!(semantic_graph(incremental), semantic_graph(oracle));
}

fn graph_path(path: &Path) -> PathBuf {
    latest_graphs()
        .lock()
        .unwrap()
        .get(&fs::canonicalize(path).unwrap())
        .cloned()
        .expect("repository was indexed")
}

fn remember_graph(path: &Path, completion: &rmcp::serde_json::Value) {
    let common_git_dir = completion["provenance"]["common_git_dir"].as_str().unwrap();
    let graph_image_id = completion["graph_image_id"].as_str().unwrap();
    latest_graphs().lock().unwrap().insert(
        fs::canonicalize(path).unwrap(),
        Path::new(common_git_dir)
            .join("graphr/v6/graphs")
            .join(format!("{graph_image_id}.db")),
    );
}

fn latest_graphs() -> &'static Mutex<HashMap<PathBuf, PathBuf>> {
    static GRAPHS: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    GRAPHS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn named_edge_count(path: &Path, source: &str, target: &str) -> i64 {
    Connection::open(graph_path(path))
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
    Connection::open(graph_path(path))
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
    let connection = Connection::open(graph_path(path)).unwrap();
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
    let connection = Connection::open(graph_path(path)).unwrap();
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

fn init_git(path: &Path) {
    git(path, &["init", "--quiet"]);
    git(
        path,
        &[
            "-c",
            "user.name=Graphr Test",
            "-c",
            "user.email=graphr@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
}

fn init_git_main(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(
        path,
        &[
            "-c",
            "user.name=Graphr Test",
            "-c",
            "user.email=graphr@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
}

fn git_init_bare(path: &Path) {
    let output = Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
}

fn git_commit(path: &Path, message: &str) {
    git(
        path,
        &[
            "-c",
            "user.name=Graphr Test",
            "-c",
            "user.email=graphr@example.invalid",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
}

fn git_output(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn assert_root_error(response: &str, code: &str, details: &[(&str, String)]) {
    let value = response_json(response);
    assert_eq!(value["result"]["isError"], true, "{response}");
    let structured = &value["result"]["structuredContent"];
    assert_eq!(structured["code"], code, "{response}");
    let actual = structured["details"].as_object();
    assert_eq!(
        actual.map_or(0, |actual| actual.len()),
        details.len(),
        "{response}"
    );
    for (key, expected) in details {
        assert_eq!(actual.unwrap().get(*key).unwrap(), expected, "{response}");
    }
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
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "graphr-e2e-linked-{label}-{}-{unique}",
        std::process::id()
    ));
    let main = root.join("main");
    let linked = root.join("linked");
    init_git_main(&main);
    fs::write(main.join("baseline.txt"), "baseline\n").unwrap();
    git(&main, &["add", "--", "baseline.txt"]);
    git_commit(&main, "baseline");
    git(
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
    git(&linked, &["add", "--", "linked.txt"]);
    git_commit(&linked, "linked");
    LinkedWorktrees { root, main, linked }
}

#[derive(Debug, Eq, PartialEq)]
struct RepositoryState {
    head: String,
    branch: String,
    refs: Vec<u8>,
    object_ids: Vec<u8>,
    index: Vec<u8>,
    worktree: BTreeMap<PathBuf, Vec<u8>>,
}

fn repository_state(root: &Path) -> RepositoryState {
    let git_path = |args: &[&str]| PathBuf::from(git_output(root, args));
    RepositoryState {
        head: git_output(root, &["rev-parse", "HEAD"]),
        branch: git_output(root, &["symbolic-ref", "--short", "HEAD"]),
        refs: git_bytes(root, &["for-each-ref", "--format=%(refname) %(objectname)"]),
        object_ids: git_bytes(root, &["rev-list", "--objects", "--all"]),
        index: fs::read(git_path(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index",
        ]))
        .unwrap(),
        worktree: worktree_bytes(root, root),
    }
}

fn git_bytes(path: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    output.stdout
}

fn worktree_bytes(root: &Path, directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            files.extend(worktree_bytes(root, &path));
        } else {
            files.insert(
                path.strip_prefix(root).unwrap().to_owned(),
                fs::read(path).unwrap(),
            );
        }
    }
    files
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
        .find(|candidate| candidate.is_file())
        .and_then(|path| fs::canonicalize(path).ok())
        .unwrap_or_else(|| panic!("cannot find {name}"))
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "condition was not reached: {}",
            path.display()
        );
        thread::yield_now();
    }
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
    repository: PathBuf,
    snapshot_id: Option<String>,
    next_id: u64,
}

impl Client {
    fn start(repository: &PathBuf) -> Self {
        let mut client = Self::start_unindexed(repository);
        client.index_and_wait("boundary");
        client
    }

    fn start_unindexed(repository: &PathBuf) -> Self {
        Self::start_unindexed_with(repository, |_| {})
    }

    fn start_unindexed_with(repository: &PathBuf, configure: impl FnOnce(&mut Command)) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphr"));
        command
            .arg("serve")
            .arg("--allow-root")
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
        let mut client = Self {
            input: child.stdin.take(),
            child,
            lines,
            repository: repository.clone(),
            snapshot_id: None,
            next_id: 10_000,
        };
        let initialized = client.request(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
        );
        assert!(initialized.contains("\"id\":1"), "{initialized}");
        client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        client
    }

    fn index_and_wait(&mut self, dependency_mode: &str) -> rmcp::serde_json::Value {
        let job_id = self.queue_index(dependency_mode);
        let value = self.wait_for_job(&job_id, "completed");
        let completion = value["result"]["structuredContent"]["state"]["completion"].clone();
        self.snapshot_id = Some(completion["snapshot_id"].as_str().unwrap().to_owned());
        remember_graph(&self.repository, &completion);
        completion
    }

    fn queue_index(&mut self, dependency_mode: &str) -> String {
        let queued = self.call(
            "index",
            rmcp::serde_json::json!({
                "worktree_root": self.repository,
                "base": "HEAD",
                "head": "HEAD",
                "target": { "kind": "worktree", "include_untracked": true },
                "dependency_mode": dependency_mode,
            }),
        );
        let queued = response_json(&queued);
        assert_eq!(
            queued["result"]["structuredContent"]["state"]["state"],
            "queued"
        );
        queued["result"]["structuredContent"]["job_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn wait_for_job(&mut self, job_id: &str, expected: &str) -> rmcp::serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "index job did not finish");
            let response = self.call(
                "index_status",
                rmcp::serde_json::json!({ "job_id": job_id }),
            );
            let value = response_json(&response);
            let state = &value["result"]["structuredContent"]["state"];
            match state["state"].as_str().unwrap() {
                actual if actual == expected => return value,
                "failed" | "cancelled" | "completed" => {
                    panic!("index job reached unexpected terminal state: {response}")
                }
                _ => {}
            }
        }
    }

    fn search(&mut self, query: &str, kind: Option<&str>) -> String {
        let mut arguments = rmcp::serde_json::json!({
            "snapshot_id": self.snapshot_id(),
            "query": query,
        });
        if let Some(kind) = kind {
            arguments["kind"] = kind.into();
        }
        self.call("search", arguments)
    }

    fn view(&mut self, node_ref: &str, depth: u32, max_nodes: u32) -> String {
        self.call(
            "view",
            rmcp::serde_json::json!({
                "snapshot_id": self.snapshot_id(),
                "node_ref": node_ref,
                "depth": depth,
                "max_nodes": max_nodes,
            }),
        )
    }

    fn changes(&mut self, depth: u32, max_nodes: u32, cursor: Option<&str>) -> String {
        let mut arguments = rmcp::serde_json::json!({
            "snapshot_id": self.snapshot_id(),
            "depth": depth,
            "max_nodes": max_nodes,
        });
        if let Some(cursor) = cursor {
            arguments["cursor"] = cursor.into();
        }
        self.call("changes", arguments)
    }

    fn call(&mut self, name: &str, arguments: rmcp::serde_json::Value) -> String {
        let id = self.next_id;
        self.next_id += 1;
        self.request(
            &rmcp::serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            })
            .to_string(),
        )
    }

    fn snapshot_id(&self) -> &str {
        self.snapshot_id.as_deref().expect("client was indexed")
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

fn response_json(response: &str) -> rmcp::serde_json::Value {
    rmcp::serde_json::from_str(response).unwrap()
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
