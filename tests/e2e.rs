use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};

#[test]
fn evidence_manifest_validation_rejects_unknown_fields_without_publication() {
    let fixture = Fixture::new();
    init_git(&fixture.path);
    fs::write(
        fixture.path.join("evidence.json"),
        format!(
            "{{\"format_version\":1,\"source_snapshot_id\":\"{}\",\"generated\":[],\"coverage\":[],\"unknown\":true}}",
            "a".repeat(64)
        ),
    )
    .unwrap();

    let output = cli_index_with_evidence(&fixture.path, "evidence.json");

    assert!(!output.status.success());
    let snapshots = fixture.path.join(".git/graphr/v6/snapshots");
    assert!(!snapshots.exists() || fs::read_dir(snapshots).unwrap().next().is_none());
}

#[test]
fn evidence_source_snapshot_rejects_mismatch_and_preserves_source() {
    let evidence = generated_evidence_fixture();
    let source_graph = graph_path(&evidence.fixture.path);
    let mut manifest: rmcp::serde_json::Value =
        rmcp::serde_json::from_slice(&fs::read(&evidence.manifest).unwrap()).unwrap();
    manifest["source_snapshot_id"] = "f".repeat(64).into();
    fs::write(
        &evidence.manifest,
        rmcp::serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let output = cli_index_with_evidence(&evidence.fixture.path, "evidence.json");

    assert!(!output.status.success());
    assert!(source_graph.exists());
    crate_graph_is_valid(&source_graph);
}

#[test]
fn evidence_source_binding_rejects_each_selected_state_change() {
    for case in [
        "source",
        "tracked-input",
        "range",
        "target",
        "dependency-mode",
        "untracked-source",
    ] {
        let evidence = generated_evidence_fixture();
        let root = &evidence.fixture.path;
        let source_graph = graph_path(root);
        let (mut base, mut head, mut target, mut include_untracked, mut dependency_mode) =
            ("HEAD", "HEAD", "worktree", true, "boundary");
        match case {
            "source" => fs::write(
                root.join("src/lib.rs"),
                "fn predicate() -> bool { false }\nfn generate() { include!(concat!(env!(\"OUT_DIR\"), \"/out.rs\")); }\n",
            )
            .unwrap(),
            "tracked-input" => {
                fs::write(root.join("schema.proto"), "message ChangedInput {}\n").unwrap()
            }
            "range" => {
                fs::write(root.join("range.txt"), "range changed\n").unwrap();
                git(root, &["add", "--", "range.txt"]);
                git_commit(root, "change selected range");
                base = "HEAD~1";
                head = "HEAD";
            }
            "target" => {
                target = "index";
                include_untracked = false;
            }
            "dependency-mode" => dependency_mode = "full",
            "untracked-source" => {
                fs::write(root.join("src/untracked.rs"), "fn untracked() {}\n").unwrap()
            }
            _ => unreachable!("fixed source-binding case"),
        }

        let output = cli_index_with_evidence_request(
            root,
            "evidence.json",
            base,
            head,
            target,
            include_untracked,
            dependency_mode,
        );

        assert!(!output.status.success(), "{case} unexpectedly succeeded");
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostics.contains("source snapshot mismatch"),
            "{case} returned the wrong failure: {diagnostics}"
        );
        assert!(source_graph.exists(), "{case} removed the source graph");
        crate_graph_is_valid(&source_graph);
    }
}

#[test]
fn evidence_cache_reuses_the_exact_verified_image() {
    let evidence = generated_evidence_fixture();

    let first = successful_evidence_index(&evidence.fixture.path);
    let second = successful_evidence_index(&evidence.fixture.path);

    assert_eq!(first["snapshot_id"], second["snapshot_id"]);
    assert_eq!(first["graph_image_id"], second["graph_image_id"]);
    assert_eq!(first["stats"]["files_parsed"], 1);
    assert_eq!(second["stats"]["files_parsed"], 0);
}

#[test]
fn generated_provenance_links_generated_calls_and_renders_verified_chain() {
    let evidence = generated_evidence_fixture();
    let completion = successful_evidence_index(&evidence.fixture.path);
    remember_graph(&evidence.fixture.path, &completion);
    let graph = graph_path(&evidence.fixture.path);
    let connection = Connection::open(graph).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM provenance_links", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM edges e JOIN nodes source ON source.id=e.source_id
                 JOIN nodes target ON target.id=e.target_id
                 WHERE source.name='generated' AND target.name='predicate' AND e.kind='CALLS'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    drop(connection);
    let mut client = Client::start_unindexed(&evidence.fixture.path);
    client.snapshot_id = Some(completion["snapshot_id"].as_str().unwrap().into());
    let snapshot_id = client.snapshot_id().to_owned();
    let inspection = client.call(
        "inspect_root",
        rmcp::serde_json::json!({
            "worktree_root": &evidence.fixture.path,
            "snapshot_id": snapshot_id,
        }),
    );
    assert!(!tool_failed(&inspection), "{inspection}");
    assert_eq!(
        response_json(&inspection)["result"]["structuredContent"]["snapshot_matches_worktree"],
        true,
        "{inspection}"
    );
    let changes = response_text(&client.changes(0, 20, None));
    assert!(
        changes.contains("basis=verified-generated-manifest"),
        "{changes}"
    );
    assert!(
        changes.contains("provenance input=\"schema.proto:1-1\""),
        "{changes}"
    );
    assert!(
        changes.contains("dynamic_evidence_status=complete"),
        "{changes}"
    );
    client.close();
}

#[test]
fn coverage_evidence_imports_scoped_rust_observations_without_dynamic_edges() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "pub fn changed() -> bool { false }\n",
    )
    .unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "src/lib.rs"]);
    git_commit(&fixture.path, "coverage baseline");
    fs::write(
        fixture.path.join("src/lib.rs"),
        "pub fn changed() -> bool { true }\n\n#[test]\nfn named() { changed(); }\n",
    )
    .unwrap();
    git(&fixture.path, &["add", "--", "src/lib.rs"]);
    git_commit(&fixture.path, "coverage change");
    let source = index_repository_request(&fixture.path, "HEAD~1", "HEAD");
    let report = rmcp::serde_json::json!({
        "type": "llvm.coverage.json.export",
        "version": "2.0.1",
        "data": [{
            "functions": [{
                "name": "ignored",
                "filenames": ["src/lib.rs"],
                "regions": [
                    [1, 1, 1, 35, 1, 0, 0, 0],
                    [4, 1, 4, 36, 0, 0, 0, 0]
                ]
            }],
            "files": [{
                "filename": "src/lib.rs",
                "branches": [[1, 1, 1, 35, 1, 0, 0, 0, 4]]
            }]
        }]
    });
    let report_bytes = rmcp::serde_json::to_vec(&report).unwrap();
    fs::write(fixture.path.join("coverage.json"), &report_bytes).unwrap();
    fs::write(
        fixture.path.join("evidence.json"),
        rmcp::serde_json::to_vec(&rmcp::serde_json::json!({
            "format_version": 1,
            "source_snapshot_id": source["snapshot_id"],
            "generated": [],
            "coverage": [{
                "format": "llvm",
                "path": "coverage.json",
                "blake3": blake3::hash(&report_bytes).to_hex().to_string(),
                "run_label": "rust-run",
                "test_name": "named"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = cli_index_with_evidence_request(
        &fixture.path,
        "evidence.json",
        "HEAD~1",
        "HEAD",
        "worktree",
        true,
        "boundary",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    let completion: rmcp::serde_json::Value =
        rmcp::serde_json::from_slice(output.stdout.trim_ascii()).unwrap();
    remember_graph(&fixture.path, &completion);
    let connection = Connection::open(graph_path(&fixture.path)).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM coverage_regions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM coverage_branches", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM edges WHERE kind NOT IN ('CALLS','TEST_CALLS','CONTAINS','IMPLEMENTS','IMPLEMENTS_TRAIT')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM edges e JOIN nodes source ON source.id=e.source_id
                  JOIN nodes target ON target.id=e.target_id
                 WHERE e.kind='TEST_CALLS' AND source.name='named' AND target.name='changed'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    drop(connection);

    let mut client = Client::start_unindexed(&fixture.path);
    client.snapshot_id = Some(completion["snapshot_id"].as_str().unwrap().into());
    let snapshot_id = client.snapshot_id().to_owned();
    let inspection = client.call(
        "inspect_root",
        rmcp::serde_json::json!({
            "worktree_root": &fixture.path,
            "snapshot_id": snapshot_id,
        }),
    );
    assert!(!tool_failed(&inspection), "{inspection}");
    let mut changes = response_text(&client.changes(6, 50, None));
    let mut cursor = page_cursor(&changes, "evidence_next_cursor");
    while let Some(token) = cursor {
        let page = changes_page(&mut client, &token);
        cursor = page_cursor(&page, "evidence_next_cursor");
        changes.push_str(&page);
    }
    for expected in [
        "execution_mapping=complete",
        "dynamic_evidence_status=complete",
        "result=observed basis=llvm-coverage-json run=\"rust-run\" test=\"named\"",
        "result=not-observed basis=llvm-coverage-json run=\"rust-run\" test=\"named\"",
        "observed-branch run=\"rust-run\" test=\"named\" path=\"src/lib.rs\" line=1 arm=true count=1",
        "not-observed-branch run=\"rust-run\" test=\"named\" path=\"src/lib.rs\" line=1 arm=false count=0",
    ] {
        assert!(changes.contains(expected), "missing {expected}: {changes}");
    }
    let search = response_text(&client.search("changed", Some("function")));
    let node_ref = search.split_whitespace().next().unwrap();
    let view = response_text(&client.view(node_ref, 0, 1));
    assert!(view.contains("path=\"src/lib.rs\" lines=1"), "{view}");
    assert!(!view.contains("path=\"src/lib.rs\" lines=4"), "{view}");
    client.close();
}

#[test]
fn coverage_evidence_imports_python_contexts_with_run_scoped_arcs() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("src/lib.py"),
        "def changed_py():\n    return False\n",
    )
    .unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "src/lib.py"]);
    git_commit(&fixture.path, "python coverage baseline");
    fs::write(
        fixture.path.join("src/lib.py"),
        "def changed_py():\n    return True\n\ndef test_named_py():\n    changed_py()\n",
    )
    .unwrap();
    git(&fixture.path, &["add", "--", "src/lib.py"]);
    git_commit(&fixture.path, "python coverage change");
    let source = index_repository_request(&fixture.path, "HEAD~1", "HEAD");
    let report = rmcp::serde_json::json!({
        "meta": {"format": 3, "version": "7.10.7"},
        "files": {
            "src/lib.py": {
                "executed_lines": [1, 2, 4, 5],
                "missing_lines": [],
                "contexts": {"1": ["test_named_py"]},
                "executed_branches": [[1, 2]],
                "missing_branches": []
            }
        }
    });
    let report_bytes = rmcp::serde_json::to_vec(&report).unwrap();
    fs::write(fixture.path.join("coverage-python.json"), &report_bytes).unwrap();
    fs::write(
        fixture.path.join("evidence.json"),
        rmcp::serde_json::to_vec(&rmcp::serde_json::json!({
            "format_version": 1,
            "source_snapshot_id": source["snapshot_id"],
            "generated": [],
            "coverage": [{
                "format": "coverage_py",
                "path": "coverage-python.json",
                "blake3": blake3::hash(&report_bytes).to_hex().to_string(),
                "run_label": "python-run"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = cli_index_with_evidence_request(
        &fixture.path,
        "evidence.json",
        "HEAD~1",
        "HEAD",
        "worktree",
        true,
        "boundary",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    let completion: rmcp::serde_json::Value =
        rmcp::serde_json::from_slice(output.stdout.trim_ascii()).unwrap();
    remember_graph(&fixture.path, &completion);
    let connection = Connection::open(graph_path(&fixture.path)).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM coverage_regions WHERE test_id IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM coverage_branches WHERE test_id IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    drop(connection);

    let mut client = Client::start_unindexed(&fixture.path);
    client.snapshot_id = Some(completion["snapshot_id"].as_str().unwrap().into());
    let snapshot_id = client.snapshot_id().to_owned();
    let inspection = client.call(
        "inspect_root",
        rmcp::serde_json::json!({
            "worktree_root": &fixture.path,
            "snapshot_id": snapshot_id,
        }),
    );
    assert!(!tool_failed(&inspection), "{inspection}");
    let mut changes = response_text(&client.changes(6, 50, None));
    let mut cursor = page_cursor(&changes, "evidence_next_cursor");
    while let Some(token) = cursor {
        let page = changes_page(&mut client, &token);
        cursor = page_cursor(&page, "evidence_next_cursor");
        changes.push_str(&page);
    }
    assert!(
        changes.contains("basis=coverage-py-json run=\"python-run\" test=\"test_named_py\""),
        "{changes}"
    );
    assert!(
        changes.contains(
            "observed-branch run=\"python-run\" path=\"src/lib.py\" line=1 arm=target:2 count=1"
        ),
        "{changes}"
    );
    assert!(
        !changes.contains("observed-branch run=\"python-run\" test="),
        "{changes}"
    );
    let named = changes
        .find(
            "claim kind=changed-execution path=\"src/lib.py\" lines=1 status=complete result=observed basis=coverage-py-json run=\"python-run\" test=\"test_named_py\"",
        )
        .unwrap();
    let run_level = changes
        .find(
            "claim kind=changed-execution path=\"src/lib.py\" lines=2 status=complete result=observed basis=coverage-py-json run=\"python-run\"",
        )
        .unwrap();
    let heuristic = changes
        .find("claim kind=static-test-paths status=")
        .unwrap_or_else(|| panic!("{changes}"));
    assert!(named < run_level && run_level < heuristic, "{changes}");
    client.close();
}

#[test]
fn evidence_pagination_is_independent_bounded_and_exhaustive() {
    let fixture = Fixture::new();
    let segment = "long-evidence-path-segment-".repeat(4);
    let source_path = format!("src/{segment}/{segment}/{segment}/covered.rs");
    fs::create_dir_all(fixture.path.join(Path::new(&source_path).parent().unwrap())).unwrap();
    let source = |value: bool| {
        (1..=48)
            .map(|line| format!("pub fn changed_{line:02}() -> bool {{ {value} }}\n"))
            .collect::<String>()
    };
    fs::write(fixture.path.join(&source_path), source(false)).unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", &source_path]);
    git_commit(&fixture.path, "coverage pagination baseline");
    fs::write(fixture.path.join(&source_path), source(true)).unwrap();
    git(&fixture.path, &["add", "--", &source_path]);
    git_commit(&fixture.path, "coverage pagination change");
    let source_snapshot = index_repository_request(&fixture.path, "HEAD~1", "HEAD");
    let report = rmcp::serde_json::json!({
        "type": "llvm.coverage.json.export",
        "version": "2.0.1",
        "data": [{
            "functions": (1..=48).map(|line| rmcp::serde_json::json!({
                "name": format!("changed_{line:02}"),
                "filenames": [&source_path],
                "regions": [[line, 1, line, 36, 1, 0, 0, 0]]
            })).collect::<Vec<_>>(),
            "files": [{"filename": &source_path, "branches": []}]
        }]
    });
    let report_bytes = rmcp::serde_json::to_vec(&report).unwrap();
    fs::write(fixture.path.join("coverage.json"), &report_bytes).unwrap();
    let run_label = "é".repeat(90);
    let missing_test = "missing_test_".repeat(15);
    fs::write(
        fixture.path.join("evidence.json"),
        rmcp::serde_json::to_vec(&rmcp::serde_json::json!({
            "format_version": 1,
            "source_snapshot_id": source_snapshot["snapshot_id"],
            "generated": [],
            "coverage": [{
                "format": "llvm",
                "path": "coverage.json",
                "blake3": blake3::hash(&report_bytes).to_hex().to_string(),
                "run_label": &run_label,
                "test_name": &missing_test
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let output = cli_index_with_evidence_request(
        &fixture.path,
        "evidence.json",
        "HEAD~1",
        "HEAD",
        "worktree",
        true,
        "boundary",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    let completion: rmcp::serde_json::Value =
        rmcp::serde_json::from_slice(output.stdout.trim_ascii()).unwrap();
    remember_graph(&fixture.path, &completion);

    let mut client = Client::start_unindexed(&fixture.path);
    client.snapshot_id = Some(completion["snapshot_id"].as_str().unwrap().into());
    let snapshot_id = client.snapshot_id().to_owned();
    let inspection = client.call(
        "inspect_root",
        rmcp::serde_json::json!({
            "worktree_root": &fixture.path,
            "snapshot_id": &snapshot_id,
        }),
    );
    assert!(!tool_failed(&inspection), "{inspection}");
    let changes = capture_changes(&mut client, &snapshot_id, 6, 1);
    for section in ["files", "diff", "artifacts", "graph", "evidence"] {
        assert!(
            changes.initial.text.lines().any(|line| line == section),
            "missing {section}: {}",
            changes.initial.text
        );
    }
    assert_eq!(
        terminal_status(&changes.initial.text),
        (true, "complete".into(), "partial".into())
    );
    assert!(
        !changes.pages["evidence"].is_empty(),
        "evidence unexpectedly fit the initial page: {}",
        changes.initial.text
    );
    let mut pages = vec![&changes.initial];
    pages.extend(changes.pages["evidence"].iter().map(|(_, page)| page));
    let totals = pages
        .iter()
        .map(|page| {
            assert!(page.text.len() <= 8192, "{}", page.text.len());
            assert_page_accounting(
                &page.text,
                "evidence",
                [
                    "emitted_records",
                    "partial_records",
                    "total_records",
                    "prior_records",
                    "remaining_records",
                ],
                "evidence_next_cursor",
            )
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(totals.len(), 1, "evidence totals changed between pages");
    assert!(
        pages
            .iter()
            .any(|page| page_metric(&page.text, "evidence", "emitted_records") > 1),
        "max_nodes leaked into evidence pages"
    );
    let evidence = change_section_text(&changes, "evidence");
    assert!(evidence.contains(&run_label), "{evidence}");
    assert_eq!(
        evidence
            .lines()
            .filter(|line| line.starts_with("observed run="))
            .count(),
        48,
        "{evidence}"
    );
    let observation = evidence.find("observed run=").unwrap();
    let gap = evidence
        .find("gap category=coverage reason=missing-test-context")
        .unwrap();
    let static_paths = evidence.find("claim kind=static-test-paths").unwrap();
    assert!(observation < gap && gap < static_paths, "{evidence}");

    let first_cursor = changes.pages["evidence"][0].0.clone();
    let repeated_a = capture_query(&client.call(
        "changes",
        rmcp::serde_json::json!({
            "snapshot_id": &snapshot_id,
            "depth": 6,
            "max_nodes": 1,
            "cursor": &first_cursor
        }),
    ));
    let repeated_b = capture_query(&client.call(
        "changes",
        rmcp::serde_json::json!({
            "snapshot_id": &snapshot_id,
            "depth": 6,
            "max_nodes": 1,
            "cursor": &first_cursor
        }),
    ));
    assert_eq!(repeated_a, repeated_b);
    for (depth, max_nodes) in [(5, 1), (6, 2)] {
        let response = client.call(
            "changes",
            rmcp::serde_json::json!({
                "snapshot_id": &snapshot_id,
                "depth": depth,
                "max_nodes": max_nodes,
                "cursor": &first_cursor
            }),
        );
        assert!(
            response.contains("cursor_parameters_mismatch"),
            "{response}"
        );
    }
    let mut tampered = first_cursor.clone();
    let replacement = if tampered.ends_with('0') { "1" } else { "0" };
    tampered.replace_range(tampered.len() - 1.., replacement);
    let response = client.call(
        "changes",
        rmcp::serde_json::json!({
            "snapshot_id": &snapshot_id,
            "depth": 6,
            "max_nodes": 1,
            "cursor": tampered
        }),
    );
    assert!(response.contains("invalid changes cursor"), "{response}");

    fs::write(
        fixture.path.join(&source_path),
        format!("{}// new snapshot\n", source(true)),
    )
    .unwrap();
    client.index_and_wait("boundary");
    let response = client.changes(6, 1, Some(&first_cursor));
    assert!(response.contains("cursor_snapshot_mismatch"), "{response}");
    client.close();
}

#[test]
fn evidence_pagination_without_a_manifest_is_empty_and_not_applicable() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/lib.rs"), "pub fn unchanged() {}\n").unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "src/lib.rs"]);
    git_commit(&fixture.path, "no evidence manifest");
    fs::write(fixture.path.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();

    let mut client = Client::start(&fixture.path);
    let snapshot_id = client.snapshot_id().to_owned();
    let changes = capture_changes(&mut client, &snapshot_id, 6, 50);

    assert_eq!(terminal_status(&changes.initial.text).2, "not-applicable");
    assert!(changes.pages["evidence"].is_empty());
    assert_eq!(
        page_metadata_line(&changes.initial.text, "evidence"),
        "evidence emitted_bytes=0 total_bytes=0 prior_bytes=0 remaining_bytes=0 byte_range=0..0 starts_mid_line=false ends_mid_line=false framing_suffix_bytes=0 emitted_records=0 partial_records=0 total_records=0 prior_records=0 remaining_records=0 page_complete=true"
    );
    client.close();
}

#[test]
fn generated_evidence_chain_joins_provenance_static_calls_and_named_coverage() {
    let evidence = generated_acceptance_fixture(GeneratedAcceptanceOptions::default());
    let completion = successful_generated_acceptance_index(&evidence);
    let mut client = client_for_completion(&evidence.fixture.path, &completion);
    let snapshot_id = client.snapshot_id().to_owned();
    let changes = capture_changes(&mut client, &snapshot_id, 6, 50);
    let artifacts = change_section_text(&changes, "artifacts");
    let graph = change_section_text(&changes, "graph");
    let observations = change_section_text(&changes, "evidence");

    assert!(
        artifacts.contains("+  optional bool strict = 1;"),
        "{artifacts}"
    );
    for expected in [
        "Function encode target/debug/build/graphr-fixture/out/message.rs:1",
        "Function decode target/debug/build/graphr-fixture/out/message.rs:2",
        "Function strict_predicate src/predicate.rs:1",
    ] {
        assert!(graph.contains(expected), "missing {expected}: {graph}");
    }
    assert_eq!(
        graph
            .lines()
            .filter(|line| {
                line.contains("caller <-")
                    && (line.contains("Function encode") || line.contains("Function decode"))
            })
            .count(),
        2,
        "{graph}"
    );
    for expected in [
        "claim kind=generated-provenance status=complete result=linked basis=verified-generated-manifest output=\"target/debug/build/graphr-fixture/out/message.rs\"",
        "provenance input=\"proto/message.proto:2-2\" generator=\"src/generator.rs:2-2\" output=\"target/debug/build/graphr-fixture/out/message.rs:1-2\"",
        "includes source=\"src/lib.rs:6\" output=\"target/debug/build/graphr-fixture/out/message.rs\"",
        "claim kind=changed-execution path=\"target/debug/build/graphr-fixture/out/message.rs\" lines=1 status=complete result=observed basis=llvm-coverage-json run=\"strict-run\" test=\"strict_roundtrip\"",
        "claim kind=changed-execution path=\"target/debug/build/graphr-fixture/out/message.rs\" lines=2 status=complete result=observed basis=llvm-coverage-json run=\"strict-run\" test=\"strict_roundtrip\"",
        "observed-branch run=\"strict-run\" test=\"strict_roundtrip\" path=\"src/predicate.rs\" line=2 arm=true count=1",
    ] {
        assert!(
            observations.contains(expected),
            "missing {expected}: {observations}"
        );
    }
    assert_eq!(
        terminal_status(&changes.initial.text),
        (true, "complete".into(), "complete".into())
    );

    for (name, line) in [("encode", 1), ("decode", 2)] {
        let search = response_text(&client.search(name, Some("function")));
        let node_ref = search
            .lines()
            .find(|line| line.contains(&format!("Function {name} ")))
            .and_then(|line| line.split_ascii_whitespace().next())
            .unwrap_or_else(|| panic!("missing generated {name}: {search}"));
        let view = response_text(&client.view(node_ref, 1, 20));
        assert!(view.contains("basis=verified-generated-manifest"), "{view}");
        assert!(
            view.contains("provenance input=\"proto/message.proto:2-2\""),
            "{view}"
        );
        assert!(
            view.contains(&format!(
                "path=\"target/debug/build/graphr-fixture/out/message.rs\" lines={line} status=complete result=observed"
            )),
            "{view}"
        );
    }
    client.close();
}

#[test]
fn generated_evidence_negative_missing_decode_static_path_changes_the_chain() {
    let positive = generated_acceptance_fixture(GeneratedAcceptanceOptions::default());
    let positive_completion = successful_generated_acceptance_index(&positive);
    let mut positive_client = client_for_completion(&positive.fixture.path, &positive_completion);
    let positive_snapshot = positive_client.snapshot_id().to_owned();
    let positive_changes = capture_changes(&mut positive_client, &positive_snapshot, 6, 50);
    let positive_graph = change_section_text(&positive_changes, "graph");
    positive_client.close();

    let negative = generated_acceptance_fixture(GeneratedAcceptanceOptions {
        decode_calls_predicate: false,
        ..GeneratedAcceptanceOptions::default()
    });
    let negative_completion = successful_generated_acceptance_index(&negative);
    let mut negative_client = client_for_completion(&negative.fixture.path, &negative_completion);
    let negative_snapshot = negative_client.snapshot_id().to_owned();
    let negative_changes = capture_changes(&mut negative_client, &negative_snapshot, 6, 50);
    let negative_graph = change_section_text(&negative_changes, "graph");

    assert_eq!(
        positive_graph
            .lines()
            .filter(|line| {
                line.contains("caller <-")
                    && (line.contains("Function encode") || line.contains("Function decode"))
            })
            .count(),
        2,
        "{positive_graph}"
    );
    assert_eq!(
        negative_graph
            .lines()
            .filter(|line| {
                line.contains("caller <-")
                    && (line.contains("Function encode") || line.contains("Function decode"))
            })
            .count(),
        1,
        "{negative_graph}"
    );
    let search = response_text(&negative_client.search("decode", Some("function")));
    let decode = search.split_ascii_whitespace().next().unwrap();
    let view = response_text(&negative_client.view(decode, 1, 20));
    assert!(!view.contains("call ->"), "{view}");
    assert_ne!(positive_graph, negative_graph);
    negative_client.close();
}

#[test]
fn generated_evidence_negative_corrupt_digest_publishes_nothing() {
    let evidence = generated_acceptance_fixture(GeneratedAcceptanceOptions {
        corrupt_output_digest: true,
        ..GeneratedAcceptanceOptions::default()
    });
    let before = published_snapshots(&evidence.fixture.path);

    let output = cli_index_with_evidence_request(
        &evidence.fixture.path,
        evidence
            .manifest
            .strip_prefix(&evidence.fixture.path)
            .unwrap()
            .to_str()
            .unwrap(),
        "HEAD",
        "HEAD",
        "worktree",
        true,
        "boundary",
    );

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("generated artifact digest does not match"),
        "{:?}",
        output.stderr
    );
    assert_eq!(published_snapshots(&evidence.fixture.path), before);
}

#[test]
fn generated_evidence_negative_omitted_test_name_stays_run_level() {
    let evidence = generated_acceptance_fixture(GeneratedAcceptanceOptions {
        include_test_name: false,
        ..GeneratedAcceptanceOptions::default()
    });
    let completion = successful_generated_acceptance_index(&evidence);
    let mut client = client_for_completion(&evidence.fixture.path, &completion);
    let snapshot_id = client.snapshot_id().to_owned();
    let changes = capture_changes(&mut client, &snapshot_id, 6, 50);
    let observations = change_section_text(&changes, "evidence");
    let run_level = "claim kind=changed-execution path=\"target/debug/build/graphr-fixture/out/message.rs\" lines=1 status=complete result=observed basis=llvm-coverage-json run=\"strict-run\"";
    let named = format!("{run_level} test=\"strict_roundtrip\"");

    assert!(observations.contains(run_level), "{observations}");
    assert!(!observations.contains(&named), "{observations}");
    assert!(
        !observations
            .lines()
            .any(|line| line.contains("test=\"strict_roundtrip\"")
                && line.contains("result=observed")),
        "run-level coverage must not be promoted into a named-test observation: {observations}"
    );
    assert!(
        observations.contains(
            "claim kind=static-test-paths status=complete basis=resolved-static-call-graph"
        ),
        "{observations}"
    );
    client.close();
}

#[test]
fn generated_evidence_negative_zero_required_branch_is_not_observed() {
    let evidence = generated_acceptance_fixture(GeneratedAcceptanceOptions {
        predicate_true_count: 0,
        ..GeneratedAcceptanceOptions::default()
    });
    let completion = successful_generated_acceptance_index(&evidence);
    let mut client = client_for_completion(&evidence.fixture.path, &completion);
    let snapshot_id = client.snapshot_id().to_owned();
    let changes = capture_changes(&mut client, &snapshot_id, 6, 50);
    let observations = change_section_text(&changes, "evidence");
    let exact = "not-observed-branch run=\"strict-run\" test=\"strict_roundtrip\" path=\"src/predicate.rs\" line=2 arm=true count=0";

    assert!(observations.contains(exact), "{observations}");
    assert!(
        !observations.lines().any(|line| {
            line.starts_with("observed-branch run=\"strict-run\"")
                && line.contains("path=\"src/predicate.rs\"")
                && line.contains("arm=true")
        }),
        "{observations}"
    );
    client.close();
}

#[test]
fn mixed_evidence_gaps_keep_completed_transport_distinct_from_partial_static_evidence() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::create_dir_all(fixture.path.join("tests")).unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "pub fn ambiguous() {}\npub fn resolved() {}\npub fn changed() { resolved(); }\n",
    )
    .unwrap();
    fs::write(fixture.path.join("src/broken.rs"), "pub fn valid() {}\n").unwrap();
    fs::write(
        fixture.path.join("tests/registration.test.js"),
        "export function jsTarget() { return false; }\nexport function jsCaller() { return jsTarget(); }\ntest(\"exercise\", () => jsCaller());\n",
    )
    .unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git_commit(&fixture.path, "mixed gap baseline");
    fs::write(
        fixture.path.join("src/lib.rs"),
        "pub fn ambiguous() {}\npub fn ambiguous() {}\npub trait Runner { fn run(&self); }\npub fn resolved() {}\npub fn changed(value: &dyn Runner) {\n    resolved();\n    ambiguous();\n    value.run();\n    println!(\"macro boundary\");\n}\n",
    )
    .unwrap();
    fs::write(fixture.path.join("src/broken.rs"), "pub fn broken( {\n").unwrap();
    fs::write(
        fixture.path.join("tests/registration.test.js"),
        "export function jsTarget() { return true; }\nexport function jsCaller() { return jsTarget(); }\ntest(\"exercise\", () => jsCaller());\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/skipped.rs"),
        vec![b'x'; 2 * 1024 * 1024 + 1],
    )
    .unwrap();

    let mut client = Client::start(&fixture.path);
    let snapshot_id = client.snapshot_id().to_owned();
    let changes = capture_changes(&mut client, &snapshot_id, 6, 50);
    let graph = change_section_text(&changes, "graph");

    assert_eq!(
        terminal_status(&changes.initial.text),
        (false, "partial".into(), "not-applicable".into())
    );
    for query in changes.queries() {
        assert!(query.text.len() <= 8192, "{}", query.text.len());
    }
    for (section, cursor) in [
        ("files", "files_next_cursor"),
        ("diff", "diff_next_cursor"),
        ("artifacts", "artifacts_next_cursor"),
        ("graph", "graph_next_cursor"),
        ("evidence", "evidence_next_cursor"),
    ] {
        let terminal = changes.pages[section]
            .last()
            .map_or(&changes.initial, |(_, page)| page);
        assert!(
            page_cursor(&terminal.text, cursor).is_none(),
            "{}",
            terminal.text
        );
    }
    for expected in [
        "Function resolved src/lib.rs:4",
        "references missing=0 ambiguous=1",
        "gaps total=4 relevant=4 by_reason=oversized:1,parser-error:1,dynamic-or-unsupported-dispatch:1,macro-expansion-unavailable:1",
        "completeness content_capture=partial source_capture=partial syntax_parse=partial site_classification=complete static_model=partial evidence_capture=not-applicable provenance_model=not-applicable execution_mapping=not-applicable traversal=complete",
        "Test exercise tests/registration.test.js:3",
    ] {
        assert!(graph.contains(expected), "missing {expected}: {graph}");
    }
    let callers = graph
        .find("claim kind=affected-callers status=partial basis=resolved-static-call-graph")
        .unwrap();
    let flows = graph
        .find("claim kind=affected-flows status=partial basis=resolved-static-call-graph")
        .unwrap();
    let tests = graph
        .find("claim kind=static-test-paths status=partial basis=resolved-static-call-graph")
        .unwrap();
    assert!(callers < flows && flows < tests, "{graph}");
    assert!(graph.contains("resolved@src/lib.rs:4"), "{graph}");
    assert!(
        graph.contains("jsTarget@tests/registration.test.js:1")
            && graph
                .lines()
                .any(|line| line.contains("test <-") && line.contains("Test exercise")),
        "{graph}"
    );
    client.close();
}

#[test]
fn completeness_reports_direct_static_calls_without_legacy_fields() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "pub fn target() -> u32 { 1 }\npub fn changed() -> u32 { target() }\n",
    )
    .unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git_commit(&fixture.path, "baseline");
    fs::write(
        fixture.path.join("src/lib.rs"),
        "pub fn target() -> u32 { 2 }\npub fn changed() -> u32 { target() }\n",
    )
    .unwrap();

    let mut client = Client::start(&fixture.path);
    let output = response_text(&client.changes(6, 50, None));
    let output = complete_graph_pages(&mut client, output, 6, 50);

    for expected in [
        "languages=rust,python,javascript,typescript",
        "completeness content_capture=complete source_capture=complete syntax_parse=complete site_classification=complete static_model=complete evidence_capture=not-applicable provenance_model=not-applicable execution_mapping=not-applicable traversal=complete",
        "claim kind=affected-callers status=complete basis=resolved-static-call-graph",
        "claim kind=affected-flows status=complete basis=resolved-static-call-graph",
        "claim kind=static-test-paths status=complete basis=resolved-static-call-graph",
        "references missing=0 ambiguous=0",
        "content_complete_when_pages_exhausted=true",
        "static_evidence_status=complete",
        "dynamic_evidence_status=not-applicable",
        "traversal_complete=true",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
    assert!(!output.contains("review_complete"), "{output}");
    assert!(
        !output
            .split_once("graph\n")
            .unwrap()
            .1
            .contains("analysis_complete"),
        "{output}"
    );
    client.close();
}

#[test]
fn completeness_keeps_finished_traversal_partial_for_macro_gap() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git_commit(&fixture.path, "baseline");
    fs::write(
        fixture.path.join("src/lib.rs"),
        "pub fn changed() { println!(\"changed\"); }\n",
    )
    .unwrap();

    let mut client = Client::start(&fixture.path);
    let output = response_text(&client.changes(6, 50, None));
    let output = complete_graph_pages(&mut client, output, 6, 50);

    assert!(output.contains("traversal_complete=true"), "{output}");
    assert!(
        output.contains("static_evidence_status=partial"),
        "{output}"
    );
    assert!(output.contains("macro-expansion-unavailable:1"), "{output}");
    assert!(
        output
            .contains("claim kind=affected-flows status=partial basis=resolved-static-call-graph")
    );
    client.close();
}

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
    let text = complete_graph_pages(&mut client, response_text(&changes), 0, 20);
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
    let text = complete_graph_pages(&mut client, response_text(&changes), 1, 50);
    assert!(text.contains("dispatch"), "{changes}");
    assert!(text.contains("decorated"), "{changes}");
    client.close();
}

const TYPES: &str = r#"
    export interface Config { value: string }
"#;
const CORE: &str = r#"
    import type { Config } from "./types.js";
    function helper(config: Config) { return config.value; }
    export { helper as exposedHelper };
    export function run(config: Config) { return helper(config); }
    export class Service {
        static create() { return new Service(); }
        dispatch(config: Config) { return this.finish(config); }
        finish(config: Config) { return run(config); }
    }
    export function makeService() { return Service.create(); }
    export function misuseType() { return Config(); }
    export function shadow(run: () => void) { run(); }
"#;
const EDITED_CORE: &str = r#"
    import type { Config } from "./types.js";
    function helper(config: Config) { return config.value; }
    export { helper as exposedHelper };
    export function run(config: Config) {
        helper(config);
        return helper(config);
    }
    export class Service {
        static create() { return new Service(); }
        dispatch(config: Config) { return this.finish(config); }
        finish(config: Config) { return run(config); }
    }
    export function makeService() { return Service.create(); }
    export function misuseType() { return Config(); }
    export function shadow(run: () => void) { run(); }
"#;
const BRIDGE: &str = r#"
    export { run as execute } from "./core.js";
    export { default as ForwardedService } from "./services";
    export { Factory as ForwardedFactory } from "./not-a-class";
    export * as widgets from "./widget";
    export * from "./types.js";
"#;
const WIDGET: &str = r#"
    export default function DefaultWidget() { return <section />; }
    export function Widget() { return <div />; }
    export function div() { return null; }
"#;
const UI: &str = r#"
    import DefaultWidget, { Widget, div } from "./widget";
    import * as UI from "./widget";
    export const Panel = () =>
        <><DefaultWidget /><Widget /><UI.Widget /><div /></>;
"#;
const SCRIPT_TESTS: &str = r#"
    import { execute } from "../src/bridge";
    import { Service } from "../src/core";
    import { Panel } from "../src/ui";
    import { future } from "../src/future";

    test.only("runs", () => execute?.({ value: "test" }));
    describe("nested", () => {
        it.skip("constructs", () => new Service());
    });
    test("static factory", () => Service.create());
    test("renders", () => Panel());
    test("future", () => future());
"#;
const MODERN: &str = r#"
    import { execute } from "./bridge";
    import { exposedHelper } from "./core";
    export function invoke() { return execute({ value: "mts" }); }
    export function invokeLocalExport() {
        return exposedHelper({ value: "local" });
    }
"#;
const COMMON: &str = r#"
    const { run } = require("./core");
    const invokeCommon = () => run({ value: "cjs" });
    module.exports = { invokeCommon };
"#;
const CONSUMER: &str = r#"
    import common = require("./common.cjs");
    const consume = () => common.invokeCommon();
    export = consume;
"#;
const ENTRY: &str = r#"
    import "./bridge";
    import { duplicate } from "./collision";
    import { indexed } from "./directory";
    function bootstrap() { return true; }
    bootstrap();
    export function unresolved() { return duplicate(); }
    export function fromIndex() { return indexed(); }
"#;
const SERVICES: &str = r#"
    class DefaultService {
        static defaultCreate() { return new DefaultService(); }
    }
    class LocalService {
        static renamedCreate() { return new LocalService(); }
    }
    export default DefaultService;
    export { LocalService as RenamedService };
"#;
const CLASS_USER: &str = r#"
    import DirectoryDefault, {
        RenamedService as DirectoryRenamed,
    } from "./services";
    import IndexDefault, {
        RenamedService as IndexRenamed,
    } from "./services/index";
    import { ForwardedService } from "./bridge";
    import { AmbiguousService } from "./ambiguous-service";
    import { ForwardedFactory } from "./bridge";
    export function useDirectoryDefault() {
        return DirectoryDefault.defaultCreate();
    }
    export function useDirectoryRenamed() {
        return DirectoryRenamed.renamedCreate();
    }
    export function useIndexDefault() {
        return IndexDefault.defaultCreate();
    }
    export function useIndexRenamed() {
        return IndexRenamed.renamedCreate();
    }
    export function useForwarded() {
        return ForwardedService.defaultCreate();
    }
    export function useAmbiguousFileMethod() {
        return AmbiguousService.fileOnly();
    }
    export function useAmbiguousIndexMethod() {
        return AmbiguousService.indexOnly();
    }
    export function useForwardedFactory() {
        return ForwardedFactory.fakeCreate();
    }
"#;
const AMBIGUOUS_SERVICE_FILE: &str = r#"
    export class AmbiguousService {
        static fileOnly() { return 1; }
    }
"#;
const AMBIGUOUS_SERVICE_INDEX: &str = r#"
    export class AmbiguousService {
        static indexOnly() { return 2; }
    }
"#;
const NOT_A_CLASS: &str = r#"
    export function Factory() {
        return {
            fakeCreate() { return 3; }
        };
    }
"#;

fn write_script_fixture(root: &Path) {
    fs::create_dir_all(root.join("src/collision")).unwrap();
    fs::create_dir_all(root.join("src/directory")).unwrap();
    fs::create_dir_all(root.join("src/services")).unwrap();
    fs::create_dir_all(root.join("src/ambiguous-service")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(root.join("src/types.d.ts"), TYPES).unwrap();
    fs::write(root.join("src/core.ts"), CORE).unwrap();
    fs::write(root.join("src/bridge.js"), BRIDGE).unwrap();
    fs::write(root.join("src/widget.jsx"), WIDGET).unwrap();
    fs::write(root.join("src/ui.tsx"), UI).unwrap();
    fs::write(root.join("src/modern.mts"), MODERN).unwrap();
    fs::write(root.join("src/common.cjs"), COMMON).unwrap();
    fs::write(root.join("src/consumer.cts"), CONSUMER).unwrap();
    fs::write(root.join("src/entry.mjs"), ENTRY).unwrap();
    fs::write(root.join("src/services/index.ts"), SERVICES).unwrap();
    fs::write(root.join("src/class-user.ts"), CLASS_USER).unwrap();
    fs::write(
        root.join("src/ambiguous-service.ts"),
        AMBIGUOUS_SERVICE_FILE,
    )
    .unwrap();
    fs::write(
        root.join("src/ambiguous-service/index.ts"),
        AMBIGUOUS_SERVICE_INDEX,
    )
    .unwrap();
    fs::write(root.join("src/not-a-class.ts"), NOT_A_CLASS).unwrap();
    fs::write(root.join("tests/core.test.ts"), SCRIPT_TESTS).unwrap();
    fs::write(
        root.join("src/collision.js"),
        "export function duplicate() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/collision/index.ts"),
        "export function duplicate() { return 2; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/directory/index.ts"),
        "export function indexed() { return 3; }\n",
    )
    .unwrap();
}

#[test]
fn javascript_typescript_index_search_view_and_incremental_changes_over_mcp() {
    let incremental = Fixture::new();
    let oracle = Fixture::new();
    for root in [&incremental.path, &oracle.path] {
        write_script_fixture(root);
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
    assert_eq!(language_file_count(&incremental.path, "javascript"), 5);
    assert_eq!(language_file_count(&incremental.path, "typescript"), 13);
    for (path, language, context) in [
        ("src/bridge.js", "javascript", "javascript"),
        ("src/widget.jsx", "javascript", "javascript"),
        ("src/entry.mjs", "javascript", "javascript"),
        ("src/common.cjs", "javascript", "javascript"),
        ("src/core.ts", "typescript", "typescript"),
        ("src/types.d.ts", "typescript", "typescript"),
        ("src/modern.mts", "typescript", "typescript"),
        ("src/consumer.cts", "typescript", "typescript"),
        ("src/ui.tsx", "typescript", "tsx"),
        ("tests/core.test.ts", "typescript", "typescript"),
    ] {
        assert_eq!(
            stored_file_language_and_context(&incremental.path, path),
            (language.to_owned(), context.to_owned()),
            "{path}"
        );
    }
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "runs",
            "src/core.ts",
            "run",
            "TEST_CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "static factory",
            "src/core.ts",
            "create",
            "TEST_CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "constructs",
            "src/core.ts",
            "Service",
            "TEST_CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "renders",
            "src/ui.tsx",
            "Panel",
            "TEST_CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/ui.tsx",
            "Panel",
            "src/widget.jsx",
            "Widget",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/ui.tsx",
            "Panel",
            "src/widget.jsx",
            "DefaultWidget",
            "CALLS",
        ),
        1
    );
    assert_eq!(named_edge_count(&incremental.path, "Panel", "div"), 0);
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "future",
            "src/future.ts",
            "future",
            "TEST_CALLS",
        ),
        0
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/core.ts",
            "run",
            "src/core.ts",
            "helper",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        [
            named_edge_kind_count(
                &incremental.path,
                "src/class-user.ts",
                "useAmbiguousFileMethod",
                "src/ambiguous-service.ts",
                "fileOnly",
                "CALLS",
            ),
            named_edge_kind_count(
                &incremental.path,
                "src/class-user.ts",
                "useAmbiguousIndexMethod",
                "src/ambiguous-service/index.ts",
                "indexOnly",
                "CALLS",
            ),
            named_edge_kind_count(
                &incremental.path,
                "src/class-user.ts",
                "useForwardedFactory",
                "src/not-a-class.ts",
                "fakeCreate",
                "CALLS",
            ),
        ],
        [0, 0, 0]
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/class-user.ts",
            "useDirectoryDefault",
            "src/services/index.ts",
            "defaultCreate",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/class-user.ts",
            "useDirectoryRenamed",
            "src/services/index.ts",
            "renamedCreate",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/class-user.ts",
            "useIndexDefault",
            "src/services/index.ts",
            "defaultCreate",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/class-user.ts",
            "useIndexRenamed",
            "src/services/index.ts",
            "renamedCreate",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/class-user.ts",
            "useForwarded",
            "src/services/index.ts",
            "defaultCreate",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/entry.mjs",
            "fromIndex",
            "src/directory/index.ts",
            "indexed",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/entry.mjs",
            "src/entry.mjs",
            "src/entry.mjs",
            "bootstrap",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/core.ts",
            "makeService",
            "src/core.ts",
            "create",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/modern.mts",
            "invoke",
            "src/core.ts",
            "run",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/common.cjs",
            "invokeCommon",
            "src/core.ts",
            "run",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/consumer.cts",
            "consume",
            "src/common.cjs",
            "invokeCommon",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/modern.mts",
            "invokeLocalExport",
            "src/core.ts",
            "helper",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/entry.mjs",
            "src/entry.mjs",
            "src/bridge.js",
            "src/bridge.js",
            "IMPORTS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/bridge.js",
            "src/bridge.js",
            "src/types.d.ts",
            "src/types.d.ts",
            "IMPORTS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/bridge.js",
            "src/bridge.js",
            "src/widget.jsx",
            "src/widget.jsx",
            "IMPORTS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/core.ts",
            "src/core.ts",
            "src/types.d.ts",
            "Config",
            "IMPORTS",
        ),
        1
    );
    assert_eq!(named_edge_count(&incremental.path, "shadow", "run"), 0);
    assert_eq!(
        named_edge_count(&incremental.path, "misuseType", "Config"),
        0
    );
    assert_eq!(
        named_edge_count(&incremental.path, "unresolved", "duplicate"),
        0
    );

    for root in [&incremental.path, &oracle.path] {
        fs::write(root.join("src/core.ts"), EDITED_CORE).unwrap();
    }
    assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
    assert_eq!(
        named_edge_support_count(
            &incremental.path,
            "src/core.ts",
            "run",
            "src/core.ts",
            "helper",
            "CALLS",
        ),
        2
    );

    for root in [&incremental.path, &oracle.path] {
        fs::write(
            root.join("src/future.ts"),
            "export function future() { return undefined; }\n",
        )
        .unwrap();
    }
    assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "future",
            "src/future.ts",
            "future",
            "TEST_CALLS",
        ),
        1
    );

    for root in [&incremental.path, &oracle.path] {
        fs::rename(root.join("src/future.ts"), root.join("src/moved.ts")).unwrap();
    }
    assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "future",
            "src/moved.ts",
            "future",
            "TEST_CALLS",
        ),
        0
    );

    for root in [&incremental.path, &oracle.path] {
        fs::rename(root.join("src/moved.ts"), root.join("src/future.ts")).unwrap();
    }
    assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "future",
            "src/future.ts",
            "future",
            "TEST_CALLS",
        ),
        1
    );

    for root in [&incremental.path, &oracle.path] {
        fs::remove_file(root.join("src/future.ts")).unwrap();
    }
    assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "future",
            "src/future.ts",
            "future",
            "TEST_CALLS",
        ),
        0
    );

    for root in [&incremental.path, &oracle.path] {
        fs::write(
            root.join("src/future.ts"),
            "export function future() { return undefined; }\n",
        )
        .unwrap();
    }
    assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "tests/core.test.ts",
            "future",
            "src/future.ts",
            "future",
            "TEST_CALLS",
        ),
        1
    );

    let mut client = Client::start(&incremental.path);
    let search = client.search("Service", Some("type"));
    let search_text = response_text(&search);
    let service = search_text
        .lines()
        .find(|line| {
            let mut fields = line.split_whitespace();
            fields.next().is_some()
                && fields.next() == Some("Type")
                && fields.next() == Some("Service")
                && fields
                    .next()
                    .is_some_and(|location| location.starts_with("src/core.ts:"))
        })
        .unwrap_or_else(|| panic!("missing Service type in src/core.ts: {search_text}"));
    let node_ref = service.split_whitespace().next().unwrap();
    let view = client.view(node_ref, 2, 30);
    assert!(view.contains("dispatch"), "{view}");
    assert!(view.contains("finish"), "{view}");
    let changes = client.changes(1, 50, None);
    assert!(changes.contains("future"), "{changes}");
    assert!(changes.contains("run"), "{changes}");
    client.close();
}

#[test]
fn script_class_member_calls_require_callable_static_methods() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("src/service.ts"),
        r#"
            export abstract class Service {
                static callable() {}
                instance() {}
                static staticField = () => {};
                instanceField = () => {};
                static get getter() { return () => {}; }
                static set setter(value: () => void) {}
                abstract declared(): void;
                callInstance() { this.instance(); this.instanceField(); }
            }
            export function localStatic() { Service.callable(); }
            export function localStaticField() { Service.staticField(); }
            export function localInstance() { Service.instance(); }
            export function localInstanceField() { Service.instanceField(); }
            export function localGetter() { Service.getter(); }
            export function localSetter() { Service.setter(); }
            export function localDeclared() { Service.declared(); }
        "#,
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/ambient.d.ts"),
        "export declare class Ambient { static signature(): void; }\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/bridge.ts"),
        "export { Service as ForwardedService } from './service';\n\
         export { Ambient as ForwardedAmbient } from './ambient';\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/user.ts"),
        r#"
            import { Service } from "./service";
            import { ForwardedService, ForwardedAmbient } from "./bridge";
            export function importedStatic() { Service.callable(); }
            export function importedStaticField() { Service.staticField(); }
            export function importedInstance() { Service.instance(); }
            export function importedInstanceField() { Service.instanceField(); }
            export function importedDeclared() { Service.declared(); }
            export function forwardedStatic() { ForwardedService.callable(); }
            export function forwardedStaticField() { ForwardedService.staticField(); }
            export function forwardedInstance() { ForwardedService.instance(); }
            export function forwardedInstanceField() { ForwardedService.instanceField(); }
            export function forwardedGetter() { ForwardedService.getter(); }
            export function forwardedSignature() { ForwardedAmbient.signature(); }
        "#,
    )
    .unwrap();
    init_git(&fixture.path);
    index_repository(&fixture.path);

    let edge = |source_path, source, target_path, target| {
        named_edge_kind_count(
            &fixture.path,
            source_path,
            source,
            target_path,
            target,
            "CALLS",
        )
    };
    for (source, target) in [
        ("localStatic", "callable"),
        ("callInstance", "instance"),
        ("callInstance", "instanceField"),
        ("localStaticField", "staticField"),
    ] {
        assert_eq!(edge("src/service.ts", source, "src/service.ts", target), 1);
    }
    for (source, target) in [
        ("importedStatic", "callable"),
        ("importedStaticField", "staticField"),
        ("forwardedStatic", "callable"),
        ("forwardedStaticField", "staticField"),
    ] {
        assert_eq!(edge("src/user.ts", source, "src/service.ts", target), 1);
    }
    for (source, target) in [
        ("localInstance", "instance"),
        ("localInstanceField", "instanceField"),
        ("localGetter", "getter"),
        ("localSetter", "setter"),
        ("localDeclared", "declared"),
    ] {
        assert_eq!(edge("src/service.ts", source, "src/service.ts", target), 0);
    }
    for (source, target) in [
        ("importedInstance", "instance"),
        ("importedInstanceField", "instanceField"),
        ("importedDeclared", "declared"),
        ("forwardedInstance", "instance"),
        ("forwardedInstanceField", "instanceField"),
        ("forwardedGetter", "getter"),
    ] {
        assert_eq!(edge("src/user.ts", source, "src/service.ts", target), 0);
    }
    assert_eq!(
        edge(
            "src/user.ts",
            "forwardedSignature",
            "src/ambient.d.ts",
            "signature",
        ),
        0
    );
}

#[test]
fn script_this_calls_respect_static_and_instance_receivers() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("src/service.ts"),
        r#"
            export class Service {
                static staticOnly() {}
                instanceOnly() {}
                static shared() {}
                shared() {}
                static staticFactory = () => this.staticOnly();
                instanceFactory = () => this.instanceOnly();
                static staticFunction = function () { this.staticOnly(); };
                instanceFunction = function () { this.instanceOnly(); };
                static staticGenerator = function* () { this.staticOnly(); };
                instanceGenerator = function* () { this.instanceOnly(); };
                static staticValue = this.staticOnly();
                instanceValue = this.instanceOnly();
                static fromStatic() {
                    this.staticOnly();
                    this.instanceOnly();
                    this.shared();
                }
                fromInstance() {
                    this.instanceOnly();
                    this.staticOnly();
                    this.shared();
                }
                nestedFunction() {
                    function nested() { this.instanceOnly(); }
                    nested();
                }
                nestedObject() {
                    const object = { nestedMethod() { this.instanceOnly(); } };
                    object.nestedMethod();
                }
            }
            export class StaticBlock {
                static target() {}
                static { this.target(); }
            }
        "#,
    )
    .unwrap();
    init_git(&fixture.path);
    index_repository(&fixture.path);

    let edge = |source, target| {
        named_edge_kind_count(
            &fixture.path,
            "src/service.ts",
            source,
            "src/service.ts",
            target,
            "CALLS",
        )
    };
    assert_eq!(edge("fromStatic", "staticOnly"), 1);
    assert_eq!(edge("fromStatic", "instanceOnly"), 0);
    assert_eq!(edge("fromStatic", "shared"), 1);
    assert_eq!(edge("fromInstance", "instanceOnly"), 1);
    assert_eq!(edge("fromInstance", "staticOnly"), 0);
    assert_eq!(edge("fromInstance", "shared"), 1);
    assert_eq!(edge("staticFactory", "staticOnly"), 1);
    assert_eq!(edge("instanceFactory", "instanceOnly"), 1);
    assert_eq!(edge("staticFunction", "staticOnly"), 1);
    assert_eq!(edge("instanceFunction", "instanceOnly"), 1);
    assert_eq!(edge("staticGenerator", "staticOnly"), 1);
    assert_eq!(edge("instanceGenerator", "instanceOnly"), 1);
    assert_eq!(edge("Service", "staticOnly"), 1);
    assert_eq!(edge("Service", "instanceOnly"), 1);
    assert_eq!(edge("nested", "instanceOnly"), 0);
    assert_eq!(edge("nestedMethod", "instanceOnly"), 0);
    assert_eq!(edge("StaticBlock", "target"), 1);
}

#[test]
fn javascript_direct_class_reexport_methods_match_fresh_graph_after_targeted_edits() {
    const FIRST_BEFORE: &str = "export class First { static before() {} static create() {} }\n";
    const FIRST_AFTER: &str = "export class First { static after() {} static create() {} }\n";
    const SECOND: &str = "export class Second { static create() {} }\n";
    const BRIDGE_BEFORE: &str =
        "export { First as TargetEdited, First as Retargeted } from './first';\n";
    const BRIDGE_AFTER: &str = "export { First as TargetEdited } from './first';\n\
         export { Second as Retargeted } from './second';\n";
    const USER: &str = r#"
        import { TargetEdited, Retargeted } from "./bridge";
        export function useTargetEdited() { return TargetEdited.after(); }
        export function useRetargeted() { return Retargeted.create(); }
    "#;

    let incremental = Fixture::new();
    let oracle = Fixture::new();
    for root in [&incremental.path, &oracle.path] {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/first.ts"), FIRST_BEFORE).unwrap();
        fs::write(root.join("src/second.ts"), SECOND).unwrap();
        fs::write(root.join("src/bridge.ts"), BRIDGE_BEFORE).unwrap();
        fs::write(root.join("src/user.ts"), USER).unwrap();
        init_git(root);
    }
    index_repository(&incremental.path);
    for root in [&incremental.path, &oracle.path] {
        fs::write(root.join("src/first.ts"), FIRST_AFTER).unwrap();
        fs::write(root.join("src/bridge.ts"), BRIDGE_AFTER).unwrap();
    }
    index_repository(&incremental.path);
    index_repository(&oracle.path);
    assert_eq!(
        [
            named_edge_kind_count(
                &incremental.path,
                "src/user.ts",
                "useTargetEdited",
                "src/first.ts",
                "after",
                "CALLS",
            ),
            named_edge_kind_count(
                &incremental.path,
                "src/user.ts",
                "useRetargeted",
                "src/second.ts",
                "create",
                "CALLS",
            ),
            named_edge_kind_count(
                &incremental.path,
                "src/user.ts",
                "useRetargeted",
                "src/first.ts",
                "create",
                "CALLS",
            ),
        ],
        [1, 1, 0]
    );
    assert_eq!(
        semantic_graph(&incremental.path),
        semantic_graph(&oracle.path)
    );
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
    let graph = complete.split_once("graph\n").unwrap().1;
    assert!(graph.contains("file-mapped src/lib.rs"), "{changed}");
    assert!(graph.contains("unmapped_ranges=0"), "{changed}");
    assert!(!graph.contains("removed_symbol"), "{changed}");
    assert!(!graph.contains("ignored_symbol"), "{changed}");
    assert!(text.len() <= 8192, "{}", text.len());
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
    let boundary = complete_review_pages(&mut client, boundary, 1, 10);
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
    let full = complete_review_pages(&mut client, full, 0, 10);
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

const LAYERED_PATH: &str =
    "src/complete_target_state/layered_source_with_committed_staged_and_unstaged_changes.rs";
const BEFORE_RENAME_PATH: &str = "src/complete_target_state/source_before_the_committed_rename.rs";
const AFTER_RENAME_PATH: &str = "src/complete_target_state/source_after_the_committed_rename.rs";
const STAGED_ADD_PATH: &str = "src/complete_target_state/source_added_in_the_staged_layer.rs";
const STAGED_DELETE_PATH: &str = "src/complete_target_state/source_deleted_in_the_staged_layer.rs";
const UNSTAGED_PATH: &str = "src/complete_target_state/source_modified_in_the_unstaged_layer.rs";
const UNTRACKED_PATH: &str = "src/complete_target_state/source_only_in_the_untracked_layer.rs";
const STAGED_MARKDOWN_PATH: &str =
    "docs/complete_target_state/requirements_changed_in_the_staged_layer.md";
const UNSTAGED_MARKDOWN_PATH: &str =
    "docs/complete_target_state/requirements_changed_in_the_unstaged_layer.md";
const UNTRACKED_TSV_PATH: &str =
    "tests/fixtures/complete_target_state/registry_only_in_the_untracked_layer.tsv";

#[test]
fn commit_index_and_worktree_targets_preserve_status_layers_and_artifacts() {
    let fixture = complete_target_state_fixture();
    let mut client = Client::start_unindexed(&fixture.path);

    let commit = client.index_target_and_wait(
        "HEAD~1",
        "HEAD",
        rmcp::serde_json::json!({ "kind": "commit" }),
    );
    let commit_snapshot = client.snapshot_id().to_owned();
    let commit_changes = capture_changes(&mut client, &commit_snapshot, 6, 50);
    assert_snapshot_provenance(
        &commit,
        &commit_changes,
        rmcp::serde_json::json!({ "kind": "commit" }),
        2,
        &["committed"],
    );
    assert_content_completion(&commit_changes, true);
    assert_change_manifest(
        &commit_changes,
        &[
            format!(
                "changed source rust {LAYERED_PATH} status=modified additions=2 deletions=1 layers=committed"
            ),
            format!(
                "renamed source rust {BEFORE_RENAME_PATH} -> {AFTER_RENAME_PATH} additions=0 deletions=0 layers=committed"
            ),
        ],
    );
    assert_graph_records(
        &commit_changes,
        &[
            ("Function", "committed_layered_symbol", LAYERED_PATH, 1),
            ("Function", "committed_helper_symbol", LAYERED_PATH, 2),
            ("Function", "renamed_symbol", AFTER_RENAME_PATH, 1),
        ],
    );
    assert_eq!(
        page_metric(&commit_changes.initial.text, "artifacts", "total_records"),
        0
    );

    let index = client.index_target_and_wait(
        "HEAD~1",
        "HEAD",
        rmcp::serde_json::json!({ "kind": "index" }),
    );
    let index_snapshot = client.snapshot_id().to_owned();
    let index_changes = capture_changes(&mut client, &index_snapshot, 6, 50);
    assert_snapshot_provenance(
        &index,
        &index_changes,
        rmcp::serde_json::json!({ "kind": "index" }),
        5,
        &["committed", "staged"],
    );
    assert_content_completion(&index_changes, true);
    assert_change_manifest(
        &index_changes,
        &[
            format!(
                "changed source rust {LAYERED_PATH} status=modified additions=3 deletions=1 layers=committed,staged"
            ),
            format!(
                "renamed source rust {BEFORE_RENAME_PATH} -> {AFTER_RENAME_PATH} additions=0 deletions=0 layers=committed"
            ),
            format!("added source rust {STAGED_ADD_PATH} additions=1 deletions=0 layers=staged"),
            format!(
                "deleted source rust {STAGED_DELETE_PATH} additions=0 deletions=1 layers=staged"
            ),
            format!(
                "changed artifact text {STAGED_MARKDOWN_PATH} analyzer=markdown additions=1 deletions=1 layers=staged"
            ),
        ],
    );
    assert_graph_records(
        &index_changes,
        &[
            ("Function", "committed_layered_symbol", LAYERED_PATH, 1),
            ("Function", "committed_helper_symbol", LAYERED_PATH, 2),
            ("Function", "staged_layered_symbol", LAYERED_PATH, 3),
            ("Function", "renamed_symbol", AFTER_RENAME_PATH, 1),
            ("Function", "staged_added_symbol", STAGED_ADD_PATH, 1),
        ],
    );
    assert_semantic_records(
        &index_changes,
        &[
            format!(
                "markdown path={STAGED_MARKDOWN_PATH:?} change=added kind=requirement value=\"REQ-2\" line=3"
            ),
            format!(
                "markdown path={STAGED_MARKDOWN_PATH:?} change=removed kind=requirement value=\"REQ-1\" line=3"
            ),
        ],
    );

    let worktree = client.index_target_and_wait(
        "HEAD~1",
        "HEAD",
        rmcp::serde_json::json!({ "kind": "worktree", "include_untracked": true }),
    );
    let worktree_snapshot = client.snapshot_id().to_owned();
    let worktree_changes = capture_changes(&mut client, &worktree_snapshot, 6, 50);
    assert_snapshot_provenance(
        &worktree,
        &worktree_changes,
        rmcp::serde_json::json!({ "kind": "worktree", "include_untracked": true }),
        9,
        &["committed", "staged", "unstaged", "untracked"],
    );
    assert_content_completion(&worktree_changes, true);
    assert_change_manifest(
        &worktree_changes,
        &[
            format!(
                "changed source rust {LAYERED_PATH} status=modified additions=4 deletions=1 layers=committed,staged,unstaged"
            ),
            format!(
                "renamed source rust {BEFORE_RENAME_PATH} -> {AFTER_RENAME_PATH} additions=0 deletions=0 layers=committed"
            ),
            format!("added source rust {STAGED_ADD_PATH} additions=1 deletions=0 layers=staged"),
            format!(
                "deleted source rust {STAGED_DELETE_PATH} additions=0 deletions=1 layers=staged"
            ),
            format!(
                "changed source rust {UNSTAGED_PATH} status=modified additions=1 deletions=1 layers=unstaged"
            ),
            format!(
                "untracked source rust {UNTRACKED_PATH} additions=20 deletions=0 layers=untracked"
            ),
            format!(
                "changed artifact text {STAGED_MARKDOWN_PATH} analyzer=markdown additions=1 deletions=1 layers=staged"
            ),
            format!(
                "changed artifact text {UNSTAGED_MARKDOWN_PATH} analyzer=markdown additions=1 deletions=1 layers=unstaged"
            ),
            format!(
                "untracked artifact text {UNTRACKED_TSV_PATH} analyzer=tsv additions=49 deletions=0 layers=untracked"
            ),
        ],
    );
    assert_graph_records(
        &worktree_changes,
        &[
            ("Function", "committed_layered_symbol", LAYERED_PATH, 1),
            ("Function", "committed_helper_symbol", LAYERED_PATH, 2),
            ("Function", "staged_layered_symbol", LAYERED_PATH, 3),
            ("Function", "unstaged_layered_symbol", LAYERED_PATH, 4),
            ("Function", "renamed_symbol", AFTER_RENAME_PATH, 1),
            ("Function", "staged_added_symbol", STAGED_ADD_PATH, 1),
            ("Function", "unstaged_current_symbol", UNSTAGED_PATH, 1),
            (
                "Function",
                "untracked_symbol_00_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                1,
            ),
            (
                "Function",
                "untracked_symbol_01_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                2,
            ),
            (
                "Function",
                "untracked_symbol_02_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                3,
            ),
            (
                "Function",
                "untracked_symbol_03_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                4,
            ),
            (
                "Function",
                "untracked_symbol_04_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                5,
            ),
            (
                "Function",
                "untracked_symbol_05_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                6,
            ),
            (
                "Function",
                "untracked_symbol_06_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                7,
            ),
            (
                "Function",
                "untracked_symbol_07_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                8,
            ),
            (
                "Function",
                "untracked_symbol_08_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                9,
            ),
            (
                "Function",
                "untracked_symbol_09_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                10,
            ),
            (
                "Function",
                "untracked_symbol_10_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                11,
            ),
            (
                "Function",
                "untracked_symbol_11_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                12,
            ),
            (
                "Function",
                "untracked_symbol_12_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                13,
            ),
            (
                "Function",
                "untracked_symbol_13_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                14,
            ),
            (
                "Function",
                "untracked_symbol_14_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                15,
            ),
            (
                "Function",
                "untracked_symbol_15_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                16,
            ),
            (
                "Function",
                "untracked_symbol_16_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                17,
            ),
            (
                "Function",
                "untracked_symbol_17_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                18,
            ),
            (
                "Function",
                "untracked_symbol_18_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                19,
            ),
            (
                "Function",
                "untracked_symbol_19_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                20,
            ),
        ],
    );
    let mut semantic_records = vec![
        format!(
            "markdown path={STAGED_MARKDOWN_PATH:?} change=added kind=requirement value=\"REQ-2\" line=3"
        ),
        format!(
            "markdown path={STAGED_MARKDOWN_PATH:?} change=removed kind=requirement value=\"REQ-1\" line=3"
        ),
        format!(
            "markdown path={UNSTAGED_MARKDOWN_PATH:?} change=added kind=requirement value=\"DOC-2\" line=3"
        ),
        format!(
            "markdown path={UNSTAGED_MARKDOWN_PATH:?} change=removed kind=requirement value=\"DOC-1\" line=3"
        ),
        format!(
            "tsv path={UNTRACKED_TSV_PATH:?} kind=key key_basis=first-column old=null new=\"id\""
        ),
        format!("tsv path={UNTRACKED_TSV_PATH:?} kind=schema old=[] new=[\"id\", \"value\"]"),
    ];
    semantic_records.extend((0..48).map(|index| {
        format!(
            "tsv path={UNTRACKED_TSV_PATH:?} change=added kind=row key=\"row-{index:02}\" occurrence=1 line={}",
            index + 2
        )
    }));
    assert_semantic_records(&worktree_changes, &semantic_records);
    for label in [
        "files_next_cursor",
        "diff_next_cursor",
        "artifacts_next_cursor",
        "graph_next_cursor",
    ] {
        assert!(
            page_cursor(&worktree_changes.initial.text, label).is_some(),
            "{label} unexpectedly terminal: {}",
            worktree_changes.initial.text
        );
    }
    assert_ne!(commit_snapshot, index_snapshot);
    assert_ne!(index_snapshot, worktree_snapshot);
    client.close();
}

#[test]
fn editing_after_publication_never_mutates_existing_queries_or_cursors() {
    let fixture = complete_target_state_fixture();
    let mut client = Client::start_unindexed(&fixture.path);
    client.index_target_and_wait(
        "HEAD~1",
        "HEAD",
        rmcp::serde_json::json!({ "kind": "worktree", "include_untracked": true }),
    );
    let old_snapshot = client.snapshot_id().to_owned();
    let old_search = capture_query(&client.search(
        "untracked_symbol_00_with_complete_target_state_evidence",
        Some("function"),
    ));
    let old_node = old_search
        .text
        .split_ascii_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let old_view = capture_query(&client.view(&old_node, 6, 50));
    let old_changes = capture_changes(&mut client, &old_snapshot, 6, 50);
    for label in [
        "files_next_cursor",
        "diff_next_cursor",
        "artifacts_next_cursor",
        "graph_next_cursor",
    ] {
        assert!(
            page_cursor(&old_changes.initial.text, label).is_some(),
            "{label} unexpectedly terminal: {}",
            old_changes.initial.text
        );
    }

    fs::write(
        fixture.path.join(LAYERED_PATH),
        format!(
            "{}pub fn post_publication_symbol() {{}}\n",
            layered_source(3)
        ),
    )
    .unwrap();
    fs::write(
        fixture
            .path
            .join("src/complete_target_state/live_added_after_publication.rs"),
        "pub fn live_added_after_publication_symbol() {}\n",
    )
    .unwrap();
    let live_renamed_path =
        "src/complete_target_state/source_renamed_after_snapshot_publication.rs";
    git(
        &fixture.path,
        &["mv", "--", AFTER_RENAME_PATH, live_renamed_path],
    );
    fs::remove_file(fixture.path.join(STAGED_ADD_PATH)).unwrap();

    let repeated_search = capture_query(&client.call(
        "search",
        rmcp::serde_json::json!({
            "snapshot_id": &old_snapshot,
            "query": "untracked_symbol_00_with_complete_target_state_evidence",
            "kind": "function",
        }),
    ));
    assert_eq!(repeated_search, old_search);
    let repeated_view = capture_query(&client.call(
        "view",
        rmcp::serde_json::json!({
            "snapshot_id": &old_snapshot,
            "node_ref": &old_node,
            "depth": 6,
            "max_nodes": 50,
        }),
    ));
    assert_eq!(repeated_view, old_view);
    let repeated_initial = capture_query(&client.call(
        "changes",
        rmcp::serde_json::json!({
            "snapshot_id": &old_snapshot,
            "depth": 6,
            "max_nodes": 50,
        }),
    ));
    assert_eq!(repeated_initial, old_changes.initial);
    for (cursor, expected) in old_changes.cursor_queries() {
        let repeated = capture_query(&client.call(
            "changes",
            rmcp::serde_json::json!({
                "snapshot_id": &old_snapshot,
                "depth": 6,
                "max_nodes": 50,
                "cursor": cursor,
            }),
        ));
        assert_eq!(&repeated, expected, "old cursor changed: {cursor}");
    }

    let divergent = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({
            "worktree_root": &fixture.path,
            "snapshot_id": &old_snapshot,
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

    client.index_target_and_wait(
        "HEAD~1",
        "HEAD",
        rmcp::serde_json::json!({ "kind": "worktree", "include_untracked": true }),
    );
    let new_snapshot = client.snapshot_id().to_owned();
    assert_ne!(new_snapshot, old_snapshot);
    for (query, expected) in [
        ("post_publication_symbol", "post_publication_symbol"),
        (
            "live_added_after_publication_symbol",
            "live_added_after_publication_symbol",
        ),
        ("renamed_symbol", live_renamed_path),
    ] {
        let result = response_text(&client.search(query, Some("function")));
        assert!(result.contains(expected), "missing {expected}: {result}");
    }
    let deleted = response_text(&client.search("staged_added_symbol", Some("function")));
    assert!(!deleted.contains("staged_added_symbol"), "{deleted}");
    let new_changes = capture_changes(&mut client, &new_snapshot, 6, 50);
    assert_change_manifest(
        &new_changes,
        &[
            format!(
                "changed source rust {LAYERED_PATH} status=modified additions=5 deletions=1 layers=committed,staged,unstaged"
            ),
            format!(
                "renamed source rust {BEFORE_RENAME_PATH} -> {live_renamed_path} additions=0 deletions=0 layers=committed,staged"
            ),
            format!(
                "deleted source rust {STAGED_DELETE_PATH} additions=0 deletions=1 layers=staged"
            ),
            format!(
                "changed source rust {UNSTAGED_PATH} status=modified additions=1 deletions=1 layers=unstaged"
            ),
            format!(
                "untracked source rust {UNTRACKED_PATH} additions=20 deletions=0 layers=untracked"
            ),
            "untracked source rust src/complete_target_state/live_added_after_publication.rs additions=1 deletions=0 layers=untracked".to_owned(),
            format!(
                "changed artifact text {STAGED_MARKDOWN_PATH} analyzer=markdown additions=1 deletions=1 layers=staged"
            ),
            format!(
                "changed artifact text {UNSTAGED_MARKDOWN_PATH} analyzer=markdown additions=1 deletions=1 layers=unstaged"
            ),
            format!(
                "untracked artifact text {UNTRACKED_TSV_PATH} analyzer=tsv additions=49 deletions=0 layers=untracked"
            ),
        ],
    );
    assert_graph_records(
        &new_changes,
        &[
            ("Function", "committed_layered_symbol", LAYERED_PATH, 1),
            ("Function", "committed_helper_symbol", LAYERED_PATH, 2),
            ("Function", "staged_layered_symbol", LAYERED_PATH, 3),
            ("Function", "unstaged_layered_symbol", LAYERED_PATH, 4),
            ("Function", "post_publication_symbol", LAYERED_PATH, 5),
            ("Function", "renamed_symbol", live_renamed_path, 1),
            ("Function", "unstaged_current_symbol", UNSTAGED_PATH, 1),
            (
                "Function",
                "live_added_after_publication_symbol",
                "src/complete_target_state/live_added_after_publication.rs",
                1,
            ),
            (
                "Function",
                "untracked_symbol_00_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                1,
            ),
            (
                "Function",
                "untracked_symbol_01_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                2,
            ),
            (
                "Function",
                "untracked_symbol_02_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                3,
            ),
            (
                "Function",
                "untracked_symbol_03_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                4,
            ),
            (
                "Function",
                "untracked_symbol_04_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                5,
            ),
            (
                "Function",
                "untracked_symbol_05_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                6,
            ),
            (
                "Function",
                "untracked_symbol_06_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                7,
            ),
            (
                "Function",
                "untracked_symbol_07_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                8,
            ),
            (
                "Function",
                "untracked_symbol_08_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                9,
            ),
            (
                "Function",
                "untracked_symbol_09_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                10,
            ),
            (
                "Function",
                "untracked_symbol_10_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                11,
            ),
            (
                "Function",
                "untracked_symbol_11_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                12,
            ),
            (
                "Function",
                "untracked_symbol_12_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                13,
            ),
            (
                "Function",
                "untracked_symbol_13_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                14,
            ),
            (
                "Function",
                "untracked_symbol_14_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                15,
            ),
            (
                "Function",
                "untracked_symbol_15_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                16,
            ),
            (
                "Function",
                "untracked_symbol_16_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                17,
            ),
            (
                "Function",
                "untracked_symbol_17_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                18,
            ),
            (
                "Function",
                "untracked_symbol_18_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                19,
            ),
            (
                "Function",
                "untracked_symbol_19_with_complete_target_state_evidence",
                UNTRACKED_PATH,
                20,
            ),
        ],
    );

    let node_mismatch = client.view(&old_node, 6, 50);
    assert_tool_error_code(&node_mismatch, "node_snapshot_mismatch");
    for (cursor, _) in old_changes.cursor_queries() {
        let snapshot_mismatch = client.call(
            "changes",
            rmcp::serde_json::json!({
                "snapshot_id": &new_snapshot,
                "depth": 6,
                "max_nodes": 50,
                "cursor": cursor,
            }),
        );
        assert_tool_error_code(&snapshot_mismatch, "cursor_snapshot_mismatch");
        let depth_mismatch = client.call(
            "changes",
            rmcp::serde_json::json!({
                "snapshot_id": &old_snapshot,
                "depth": 5,
                "max_nodes": 50,
                "cursor": cursor,
            }),
        );
        assert_tool_error_code(&depth_mismatch, "cursor_parameters_mismatch");
        let nodes_mismatch = client.call(
            "changes",
            rmcp::serde_json::json!({
                "snapshot_id": &old_snapshot,
                "depth": 6,
                "max_nodes": 49,
                "cursor": cursor,
            }),
        );
        assert_tool_error_code(&nodes_mismatch, "cursor_parameters_mismatch");
    }
    client.close();
}

fn complete_target_state_fixture() -> Fixture {
    let fixture = Fixture::new();
    for directory in [
        "src/complete_target_state",
        "docs/complete_target_state",
        "tests/fixtures/complete_target_state",
    ] {
        fs::create_dir_all(fixture.path.join(directory)).unwrap();
    }
    fs::write(fixture.path.join(LAYERED_PATH), layered_source(0)).unwrap();
    fs::write(
        fixture.path.join(BEFORE_RENAME_PATH),
        "pub fn renamed_symbol() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join(STAGED_DELETE_PATH),
        "pub fn staged_deleted_symbol() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join(UNSTAGED_PATH),
        "pub fn unstaged_base_symbol() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join(STAGED_MARKDOWN_PATH),
        "# Stable heading\n\nREQ-1\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join(UNSTAGED_MARKDOWN_PATH),
        "# Stable heading\n\nDOC-1\n",
    )
    .unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git_commit(&fixture.path, "baseline");

    fs::write(fixture.path.join(LAYERED_PATH), layered_source(1)).unwrap();
    git(
        &fixture.path,
        &["mv", "--", BEFORE_RENAME_PATH, AFTER_RENAME_PATH],
    );
    git(&fixture.path, &["add", "--", LAYERED_PATH]);
    git_commit(&fixture.path, "committed layer");

    fs::write(fixture.path.join(LAYERED_PATH), layered_source(2)).unwrap();
    fs::write(
        fixture.path.join(STAGED_ADD_PATH),
        "pub fn staged_added_symbol() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join(STAGED_MARKDOWN_PATH),
        "# Stable heading\n\nREQ-2\n",
    )
    .unwrap();
    git(
        &fixture.path,
        &[
            "add",
            "--",
            LAYERED_PATH,
            STAGED_ADD_PATH,
            STAGED_MARKDOWN_PATH,
        ],
    );
    git(&fixture.path, &["rm", "--quiet", "--", STAGED_DELETE_PATH]);

    fs::write(fixture.path.join(LAYERED_PATH), layered_source(3)).unwrap();
    fs::write(
        fixture.path.join(UNSTAGED_PATH),
        "pub fn unstaged_current_symbol() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join(UNSTAGED_MARKDOWN_PATH),
        "# Stable heading\n\nDOC-2\n",
    )
    .unwrap();
    fs::write(fixture.path.join(UNTRACKED_PATH), untracked_source()).unwrap();
    fs::write(fixture.path.join(UNTRACKED_TSV_PATH), untracked_tsv()).unwrap();
    fixture
}

fn layered_source(layer: usize) -> String {
    let mut lines = vec![if layer == 0 {
        "pub fn layered_base_symbol() {}\n"
    } else {
        "pub fn committed_layered_symbol() { committed_helper_symbol(); }\n"
    }];
    if layer > 0 {
        lines.push("pub fn committed_helper_symbol() {}\n");
    }
    if layer > 1 {
        lines.push("pub fn staged_layered_symbol() { committed_helper_symbol(); }\n");
    }
    if layer > 2 {
        lines.push("pub fn unstaged_layered_symbol() { staged_layered_symbol(); }\n");
    }
    lines.concat()
}

fn untracked_source() -> String {
    (0..20)
        .map(|index| {
            format!(
                "pub fn untracked_symbol_{index:02}_with_complete_target_state_evidence() {{}}\n"
            )
        })
        .collect()
}

fn untracked_tsv() -> String {
    let mut output = String::from("id\tvalue\n");
    for index in 0..48 {
        output.push_str(&format!("row-{index:02}\tvalue-{index:02}\n"));
    }
    output
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueryCapture {
    text: String,
    provenance: rmcp::serde_json::Value,
}

#[derive(Debug, Eq, PartialEq)]
struct ChangesCapture {
    initial: QueryCapture,
    pages: BTreeMap<&'static str, Vec<(String, QueryCapture)>>,
}

impl ChangesCapture {
    fn queries(&self) -> impl Iterator<Item = &QueryCapture> {
        std::iter::once(&self.initial).chain(self.pages.values().flatten().map(|(_, page)| page))
    }

    fn cursor_queries(&self) -> impl Iterator<Item = (&str, &QueryCapture)> {
        self.pages
            .values()
            .flatten()
            .map(|(cursor, capture)| (cursor.as_str(), capture))
    }
}

fn capture_query(response: &str) -> QueryCapture {
    let value = response_json(response);
    assert_ne!(value["result"]["isError"], true, "{response}");
    QueryCapture {
        text: response_text(response),
        provenance: value["result"]["structuredContent"]["provenance"].clone(),
    }
}

fn capture_changes(
    client: &mut Client,
    snapshot_id: &str,
    depth: u32,
    max_nodes: u32,
) -> ChangesCapture {
    capture_changes_in_order(
        client,
        snapshot_id,
        depth,
        max_nodes,
        [
            ("files", "files_next_cursor"),
            ("diff", "diff_next_cursor"),
            ("artifacts", "artifacts_next_cursor"),
            ("graph", "graph_next_cursor"),
            ("evidence", "evidence_next_cursor"),
        ],
    )
}

fn capture_changes_in_order<const N: usize>(
    client: &mut Client,
    snapshot_id: &str,
    depth: u32,
    max_nodes: u32,
    sections: [(&'static str, &'static str); N],
) -> ChangesCapture {
    let initial = capture_query(&client.call(
        "changes",
        rmcp::serde_json::json!({
            "snapshot_id": snapshot_id,
            "depth": depth,
            "max_nodes": max_nodes,
        }),
    ));
    assert_eq!(initial.provenance["snapshot_id"], snapshot_id);
    let status = terminal_status(&initial.text);
    let mut pages = BTreeMap::new();
    for (section, cursor_label) in sections {
        let mut section_pages = Vec::new();
        let mut cursor = page_cursor(&initial.text, cursor_label);
        assert_eq!(
            page_field(page_metadata_line(&initial.text, section), "page_complete") == "true",
            cursor.is_none(),
            "{}",
            initial.text
        );
        while let Some(token) = cursor {
            let page = capture_query(&client.call(
                "changes",
                rmcp::serde_json::json!({
                    "snapshot_id": snapshot_id,
                    "depth": depth,
                    "max_nodes": max_nodes,
                    "cursor": &token,
                }),
            ));
            assert_eq!(page.provenance, initial.provenance);
            assert!(
                page.text.starts_with(&format!("{section}\n")),
                "{}",
                page.text
            );
            assert_eq!(
                terminal_status(&page.text),
                status,
                "completion changed between pages: {}",
                page.text,
            );
            let next = page_cursor(&page.text, cursor_label);
            assert_eq!(
                page_field(page_metadata_line(&page.text, section), "page_complete") == "true",
                next.is_none(),
                "{}",
                page.text
            );
            section_pages.push((token, page));
            cursor = next;
        }
        pages.insert(section, section_pages);
    }
    ChangesCapture { initial, pages }
}

fn terminal_status(output: &str) -> (bool, String, String) {
    let field = |name: &str| {
        output
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
    };
    (
        field("content_complete_when_pages_exhausted=")
            .parse()
            .unwrap(),
        field("static_evidence_status=").to_owned(),
        field("dynamic_evidence_status=").to_owned(),
    )
}

fn assert_content_completion(changes: &ChangesCapture, expected: bool) {
    assert_eq!(
        terminal_status(&changes.initial.text).0,
        expected,
        "{}",
        changes.initial.text
    );
    for (_, page) in changes.cursor_queries() {
        assert_eq!(terminal_status(&page.text).0, expected, "{}", page.text);
    }
}

fn change_section_text(changes: &ChangesCapture, section: &str) -> String {
    let next = match section {
        "files" => "diff",
        "diff" => "artifacts",
        "artifacts" => "graph",
        "graph" => "evidence",
        "evidence" => "content_complete_when_pages_exhausted=",
        _ => unreachable!(),
    };
    let header = format!("{section}\n");
    let start = changes.initial.text.find(&header).unwrap() + header.len();
    let end = changes.initial.text[start..]
        .find(&format!("\n{next}"))
        .map_or(changes.initial.text.len(), |offset| start + offset);
    let mut output = changes.initial.text[start..end].to_owned();
    for (_, page) in &changes.pages[section] {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&page.text);
    }
    output
}

fn assert_change_manifest(changes: &ChangesCapture, expected: &[String]) {
    let mut actual = change_section_text(changes, "files")
        .lines()
        .filter(|line| {
            [
                "added ",
                "changed ",
                "deleted ",
                "renamed ",
                "type-changed ",
                "unmerged ",
                "untracked ",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut expected = expected.to_vec();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

fn assert_semantic_records(changes: &ChangesCapture, expected: &[String]) {
    let mut actual = change_section_text(changes, "artifacts")
        .lines()
        .filter(|line| line.starts_with("markdown ") || line.starts_with("tsv "))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut expected = expected.to_vec();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
enum GraphRecord {
    Symbol(String, String, String, u32),
    File(String, u32),
}

fn parse_graph_records(output: &str) -> Result<BTreeSet<GraphRecord>, String> {
    let mut records = BTreeSet::new();
    for line in output.lines() {
        if let Some(record) = parse_graph_line(line)? {
            records.insert(record);
        }
    }
    Ok(records)
}

#[test]
fn compact_query_records_reject_unexpected_nodes() {
    let snapshot = "a".repeat(64);
    let output = format!(
        "n1:{snapshot}:00000001:1:1 Function left_staged_symbol src/left.rs:1\nn1:{snapshot}:00000001:1:2 Function unexpected_symbol src/unexpected.rs:7\n"
    );
    let expected = BTreeSet::from([GraphRecord::Symbol(
        "Function".into(),
        "left_staged_symbol".into(),
        "src/left.rs".into(),
        1,
    )]);

    assert!(output.contains("left_staged_symbol"));
    assert!(!output.contains("right_staged_symbol"));
    assert!(
        assert_exact_query_records(&output, &expected).is_err(),
        "unexpected compact node was accepted"
    );
}

fn assert_exact_query_records(
    output: &str,
    expected: &BTreeSet<GraphRecord>,
) -> Result<(), String> {
    let actual = parse_graph_records(output)?;
    (actual == *expected)
        .then_some(())
        .ok_or_else(|| format!("compact node records differ: {actual:?}"))
}

fn parse_graph_line(line: &str) -> Result<Option<GraphRecord>, String> {
    if let Some(risk) = line.strip_prefix("  risk ") {
        let (score, mut node) = risk.split_once(' ').ok_or_else(|| invalid_graph(line))?;
        if !valid_score(score) {
            return Err(invalid_graph(line));
        }
        node = ["no-static-test-path ", "indirect-test-covered "]
            .iter()
            .find_map(|tag| node.strip_prefix(tag))
            .unwrap_or(node);
        return parse_graph_node(node).map(Some);
    }
    for relation in [
        "test <-",
        "caller <-",
        "in <-",
        "impl <-",
        "call ->",
        "implements ->",
        "import ->",
    ] {
        if let Some(node) = line.strip_prefix(&format!("  {relation} ")) {
            if let Some(package) = node.strip_prefix("dependency-boundary package=") {
                return (!package.is_empty() && !package.contains(char::is_whitespace))
                    .then_some(None)
                    .ok_or_else(|| invalid_graph(line));
            }
            return parse_graph_node(node).map(Some);
        }
    }
    if line.starts_with("n1:") {
        return parse_graph_node(line).map(Some);
    }
    if line
        .split_ascii_whitespace()
        .any(|field| field.starts_with("n1:"))
    {
        return Err(invalid_graph(line));
    }
    is_graph_non_node(line)
        .then_some(None)
        .ok_or_else(|| invalid_graph(line))
}

fn parse_graph_node(node: &str) -> Result<GraphRecord, String> {
    let (node_ref, fields) = node.split_once(' ').ok_or_else(|| invalid_graph(node))?;
    let (kind, payload_line) = fields.split_once(' ').ok_or_else(|| invalid_graph(node))?;
    if !valid_node_ref(node_ref)
        || !matches!(kind, "File" | "Type" | "Function" | "Test")
        || payload_line.is_empty()
        || payload_line.starts_with(' ')
    {
        return Err(invalid_graph(node));
    }
    let (payload, raw_line) = payload_line
        .rsplit_once(':')
        .filter(|(payload, _)| !payload.is_empty())
        .ok_or_else(|| invalid_graph(node))?;
    let line = raw_line
        .parse::<u32>()
        .ok()
        .filter(|line| *line > 0 && raw_line == line.to_string())
        .ok_or_else(|| invalid_graph(node))?;
    if kind == "File" {
        return Ok(GraphRecord::File(payload.to_owned(), line));
    }
    let (name, path) = payload
        .split_once(' ')
        .filter(|(name, path)| !name.is_empty() && !path.is_empty() && !path.starts_with(' '))
        .ok_or_else(|| invalid_graph(node))?;
    Ok(GraphRecord::Symbol(
        kind.to_owned(),
        name.to_owned(),
        path.to_owned(),
        line,
    ))
}

fn valid_node_ref(node_ref: &str) -> bool {
    let fields = node_ref.split(':').collect::<Vec<_>>();
    let ["n1", snapshot, epoch, generation, id] = fields.as_slice() else {
        return false;
    };
    snapshot.len() == 64
        && snapshot
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && epoch.len() == 8
        && epoch.bytes().all(|byte| byte.is_ascii_hexdigit())
        && canonical_i64(generation, false)
        && canonical_i64(id, true)
}

fn canonical_i64(raw: &str, positive: bool) -> bool {
    raw.parse::<i64>().is_ok_and(|value| {
        raw == value.to_string() && if positive { value > 0 } else { value >= 0 }
    })
}

fn valid_score(score: &str) -> bool {
    score.split_once('.').is_some_and(|(whole, fraction)| {
        canonical_i64(whole, false)
            && fraction.len() == 4
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
            && whole
                .parse::<u32>()
                .ok()
                .and_then(|whole| whole.checked_mul(10_000))
                .and_then(|whole| whole.checked_add(fraction.parse().unwrap()))
                .is_some()
    })
}

fn is_graph_non_node(line: &str) -> bool {
    if line.is_empty() || line == "graph" {
        return true;
    }
    if [
        "graph emitted_bytes=",
        "risk overall=",
        "languages=",
        "completeness ",
        "gaps ",
        "references ",
        "claim ",
        "file-mapped ",
        "unmapped ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix) && line.len() > prefix.len())
    {
        return true;
    }
    if let Some(flow) = line.strip_prefix("flow ") {
        return flow
            .split_once(' ')
            .is_some_and(|(score, fields)| valid_score(score) && fields.starts_with("depth="));
    }
    line.strip_prefix("graph_next_cursor=")
        .is_some_and(|cursor| !cursor.is_empty() && !cursor.contains(char::is_whitespace))
        || matches!(
            line,
            "content_complete_when_pages_exhausted=true"
                | "content_complete_when_pages_exhausted=false"
                | "static_evidence_status=complete"
                | "static_evidence_status=partial"
                | "static_evidence_status=not-applicable"
                | "dynamic_evidence_status=complete"
                | "dynamic_evidence_status=partial"
                | "dynamic_evidence_status=not-applicable"
        )
}

fn invalid_graph(line: &str) -> String {
    format!("invalid graph line: {line}")
}

#[test]
fn graph_record_parser_preserves_ascii_spaces_inside_paths() {
    const REF: &str =
        "n1:0000000000000000000000000000000000000000000000000000000000000000:deadbeef:0:1";

    assert_eq!(
        parse_graph_records(&format!("{REF} Function one_space src/one space.rs:2")).unwrap(),
        BTreeSet::from([GraphRecord::Symbol(
            "Function".to_owned(),
            "one_space".to_owned(),
            "src/one space.rs".to_owned(),
            2,
        )])
    );
    assert_eq!(
        parse_graph_records(&format!(
            "{REF} Function repeated_spaces src/repeated  spaces.rs:3"
        ))
        .unwrap(),
        BTreeSet::from([GraphRecord::Symbol(
            "Function".to_owned(),
            "repeated_spaces".to_owned(),
            "src/repeated  spaces.rs".to_owned(),
            3,
        )])
    );
    assert_eq!(
        parse_graph_records(&format!("{REF} File src/file name.rs src/file name.rs:4")).unwrap(),
        BTreeSet::from([GraphRecord::File(
            "src/file name.rs src/file name.rs".to_owned(),
            4,
        )])
    );
}

#[test]
fn graph_record_parser_rejects_noncanonical_field_whitespace() {
    const REF: &str =
        "n1:0000000000000000000000000000000000000000000000000000000000000000:deadbeef:0:1";

    for malformed in [
        format!("{REF}  Function name src/lib.rs:1"),
        format!("{REF} Function  name src/lib.rs:1"),
        format!("{REF} Function name  src/lib.rs:1"),
        format!(" {REF} Function name src/lib.rs:1"),
        format!("{REF} Function name src/lib.rs:1 "),
    ] {
        assert!(
            parse_graph_records(&malformed).is_err(),
            "unexpectedly accepted {malformed:?}"
        );
    }
}

#[test]
fn graph_record_parser_accepts_only_complete_node_records() {
    const REF: &str =
        "n1:0000000000000000000000000000000000000000000000000000000000000000:deadbeef:0:1";

    let bare = format!("{REF} Function bare src/lib.rs:1");
    assert_eq!(
        parse_graph_records(&bare).unwrap(),
        BTreeSet::from([GraphRecord::Symbol(
            "Function".to_owned(),
            "bare".to_owned(),
            "src/lib.rs".to_owned(),
            1,
        )])
    );
    for prefix in [
        "  risk 0.1000 ",
        "  risk 0.1000 no-static-test-path ",
        "  risk 0.1000 indirect-test-covered ",
        "  test <- ",
        "  caller <- ",
        "  impl <- ",
        "  call -> ",
        "  implements -> ",
        "  import -> ",
    ] {
        let line = format!("{prefix}{REF} Function valid src/lib.rs:1");
        assert!(parse_graph_records(&line).is_ok(), "{line}");
    }
    assert!(
        parse_graph_records("  call -> dependency-boundary package=sha2")
            .unwrap()
            .is_empty()
    );

    for malformed in [
        format!("bogus {REF} Function name src/lib.rs:1"),
        "n1:ABCDEF0000000000000000000000000000000000000000000000000000000000:deadbeef:0:1 Function name src/lib.rs:1".to_owned(),
        "n1:0000000000000000000000000000000000000000000000000000000000000000:deadbeef:00:1 Function name src/lib.rs:1".to_owned(),
        "n1:0000000000000000000000000000000000000000000000000000000000000000:deadbeef:0:0 Function name src/lib.rs:1".to_owned(),
        format!("{REF} Function name"),
        format!("{REF} Function name src/lib.rs:1 trailing"),
        format!("{REF} Unknown name src/lib.rs:1"),
        format!("{REF} Function name src/lib.rs:0"),
        format!("{REF} Function name src/lib.rs:01"),
        format!("{REF} Function name src/lib.rs:not-a-line"),
        format!("  risk 1.000 {REF} Function name src/lib.rs:1"),
        format!("  risk 0.1000 unknown-tag {REF} Function name src/lib.rs:1"),
        "  call -> dependency-boundary package=".to_owned(),
        "unknown graph metadata".to_owned(),
    ] {
        assert!(
            parse_graph_records(&malformed).is_err(),
            "unexpectedly accepted {malformed}"
        );
    }
}

fn assert_graph_records(changes: &ChangesCapture, expected: &[(&str, &str, &str, u32)]) {
    let graph = change_section_text(changes, "graph");
    let actual = parse_graph_records(&graph).unwrap_or_else(|error| panic!("{error}\n{graph}"));
    let expected = expected
        .iter()
        .map(|(kind, name, path, line)| {
            GraphRecord::Symbol(
                (*kind).to_owned(),
                (*name).to_owned(),
                (*path).to_owned(),
                *line,
            )
        })
        .collect::<BTreeSet<GraphRecord>>();
    assert_eq!(actual, expected);
}

fn assert_snapshot_provenance(
    completion: &rmcp::serde_json::Value,
    changes: &ChangesCapture,
    target: rmcp::serde_json::Value,
    changed_files: usize,
    selected_layers: &[&str],
) {
    let provenance = &completion["provenance"];
    assert_eq!(provenance["target_state"], target);
    assert_eq!(provenance["changed_files"], changed_files);
    assert_eq!(
        provenance["selected_layers"],
        rmcp::serde_json::json!(selected_layers)
    );
    assert_eq!(changes.initial.provenance, *provenance);
}

fn assert_tool_error_code(response: &str, expected: &str) {
    let value = response_json(response);
    assert_eq!(value["result"]["isError"], true, "{response}");
    assert_eq!(
        value["result"]["structuredContent"]["code"], expected,
        "{response}"
    );
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
        "untracked artifact omitted image.bin analyzer=generic reason=binary",
        "files_next_cursor=",
        "artifacts_next_cursor=",
        "content_complete_when_pages_exhausted=false",
        "static_evidence_status=partial",
        "dynamic_evidence_status=not-applicable",
        "total_hunks=14",
        "changed_symbols_total=14",
        "flows_total=3",
    ] {
        assert!(initial.contains(expected), "missing {expected}: {initial}");
    }
    let file_pages =
        complete_section_pages(&mut client, initial.clone(), 6, 50, "files_next_cursor");
    assert!(
        file_pages
            .contains("untracked artifact text tests/fixtures/alias-registry.v1.tsv analyzer=tsv"),
        "{file_pages}"
    );
    assert!(!file_pages.contains("ignored.txt"), "{file_pages}");
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
            page.contains("content_complete_when_pages_exhausted=false"),
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
    assert!(artifact_pages.contains("markdown path=\"README.md\""));
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
    let changes = complete_graph_pages(&mut client, changes, 0, 50);

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
        changes.contains("content_complete_when_pages_exhausted=true"),
        "{changes}"
    );
    assert!(
        changes.contains("static_evidence_status=partial"),
        "{changes}"
    );
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

fn complete_graph_pages(
    client: &mut Client,
    initial: String,
    depth: u32,
    max_nodes: u32,
) -> String {
    complete_section_pages(client, initial, depth, max_nodes, "graph_next_cursor")
}

fn complete_review_pages(
    client: &mut Client,
    mut output: String,
    depth: u32,
    max_nodes: u32,
) -> String {
    for cursor in [
        "files_next_cursor",
        "diff_next_cursor",
        "artifacts_next_cursor",
        "graph_next_cursor",
        "evidence_next_cursor",
    ] {
        output = complete_section_pages(client, output, depth, max_nodes, cursor);
    }
    output
}

fn complete_section_pages(
    client: &mut Client,
    mut output: String,
    depth: u32,
    max_nodes: u32,
    cursor_label: &str,
) -> String {
    let mut cursor = page_cursor(&output, cursor_label);
    while let Some(token) = cursor {
        let page = response_text(&client.changes(depth, max_nodes, Some(&token)));
        cursor = page_cursor(&page, cursor_label);
        output.push_str(&page);
    }
    output
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
        .unwrap_or_else(|| panic!("missing {section} page metadata: {output}"))
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
fn two_linked_worktrees_index_concurrently_without_cross_contamination() {
    let mut worktrees = linked_worktrees("concurrent-isolation");
    let second = worktrees.add("linked-two");
    fs::write(second.join("second.txt"), "second\n").unwrap();
    git(&second, &["add", "--", "second.txt"]);
    git_commit(&second, "second linked head");

    fs::write(
        worktrees.linked.join("src/left.rs"),
        "pub fn left_staged_symbol() {}\n",
    )
    .unwrap();
    git(&worktrees.linked, &["add", "--", "src/left.rs"]);
    fs::write(
        second.join("src/right.rs"),
        "pub fn right_staged_symbol() {}\n",
    )
    .unwrap();
    git(&second, &["add", "--", "src/right.rs"]);
    let left_before = repository_state(&worktrees.linked);
    let right_before = repository_state(&second);

    let blocker = GitBlocker::new(&worktrees.root, 2);
    blocker.block();
    let mut client = Client::start_unindexed_with(&worktrees.linked, |command| {
        command.arg("--allow-root").arg(&second);
        blocker.configure(command);
    });
    let left_queued = response_json(&client.call(
        "index",
        rmcp::serde_json::json!({
            "worktree_root": &worktrees.linked,
            "base": "HEAD",
            "head": "HEAD",
            "target": { "kind": "worktree", "include_untracked": true },
            "dependency_mode": "boundary",
        }),
    ));
    let right_queued = response_json(&client.call(
        "index",
        rmcp::serde_json::json!({
            "worktree_root": &second,
            "base": "HEAD",
            "head": "HEAD",
            "target": { "kind": "worktree", "include_untracked": true },
            "dependency_mode": "boundary",
        }),
    ));
    let left_job = left_queued["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap();
    let right_job = right_queued["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap();
    assert_ne!(left_job, right_job);
    let entered = blocker.wait_for_entries(2);
    let expected_roots = BTreeSet::from([
        fs::canonicalize(&worktrees.linked).unwrap(),
        fs::canonicalize(&second).unwrap(),
    ]);
    assert_blocked_git_markers(&entered, &expected_roots)
        .unwrap_or_else(|error| panic!("{error}: {entered:?}"));
    for job_id in [left_job, right_job] {
        let status = response_json(&client.call(
            "index_status",
            rmcp::serde_json::json!({ "job_id": job_id }),
        ));
        assert_eq!(
            status["result"]["structuredContent"]["state"]["state"], "capturing",
            "both jobs must be active before release: {status}"
        );
    }

    let left_inspection = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({ "worktree_root": &worktrees.linked }),
    ));
    let right_inspection = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({ "worktree_root": &second }),
    ));
    let left_identity = &left_inspection["result"]["structuredContent"]["identity"];
    let right_identity = &right_inspection["result"]["structuredContent"]["identity"];
    for inspection in [&left_inspection, &right_inspection] {
        let root = &inspection["result"]["structuredContent"];
        assert_eq!(root["staged_paths"], 1);
        assert_eq!(root["unstaged_paths"], 0);
        assert_eq!(root["untracked_paths"], 0);
    }
    assert_eq!(
        left_identity["repository_id"],
        right_identity["repository_id"]
    );
    assert_eq!(
        left_identity["common_git_dir"],
        right_identity["common_git_dir"]
    );
    for field in [
        "workspace_id",
        "worktree_root",
        "git_dir",
        "head_oid",
        "index_path",
    ] {
        assert_ne!(left_identity[field], right_identity[field], "{field}");
    }

    blocker.release();
    let left_completed = client.wait_for_job(left_job, "completed");
    let right_completed = client.wait_for_job(right_job, "completed");
    let left_status = &left_completed["result"]["structuredContent"];
    let right_status = &right_completed["result"]["structuredContent"];
    assert_eq!(left_status["request"]["root"], *left_identity);
    assert_eq!(right_status["request"]["root"], *right_identity);
    let left = &left_status["state"]["completion"];
    let right = &right_status["state"]["completion"];
    let left_provenance = &left["provenance"];
    let right_provenance = &right["provenance"];
    assert_eq!(
        left_provenance["repository_id"],
        right_provenance["repository_id"]
    );
    assert_eq!(
        left_provenance["common_git_dir"],
        right_provenance["common_git_dir"]
    );
    for field in [
        "workspace_id",
        "snapshot_id",
        "repository_root",
        "worktree_root",
        "git_dir",
        "head_oid",
        "dirty_digest",
    ] {
        assert_ne!(left_provenance[field], right_provenance[field], "{field}");
    }
    for provenance in [left_provenance, right_provenance] {
        assert_eq!(provenance["target_state"]["kind"], "worktree");
        assert_eq!(
            provenance["selected_layers"],
            rmcp::serde_json::json!(["staged"])
        );
        assert_eq!(provenance["changed_files"], 1);
    }
    assert_ne!(left["graph_image_id"], right["graph_image_id"]);

    let left_cache = cached_snapshot(left);
    let right_cache = cached_snapshot(right);
    assert_eq!(left_cache.manifest["provenance"], *left_provenance);
    assert_eq!(right_cache.manifest["provenance"], *right_provenance);
    assert_ne!(left_cache.graph_path, right_cache.graph_path);
    assert_ne!(
        left_cache.files[&left_cache.graph_path].bytes,
        right_cache.files[&right_cache.graph_path].bytes
    );
    for cache in [&left_cache, &right_cache] {
        assert_eq!(cache.files[&cache.graph_path].mode & 0o222, 0);
        assert!(!PathBuf::from(format!("{}-wal", cache.graph_path.display())).exists());
        assert!(!PathBuf::from(format!("{}-shm", cache.graph_path.display())).exists());
    }

    let left_snapshot = left["snapshot_id"].as_str().unwrap();
    let right_snapshot = right["snapshot_id"].as_str().unwrap();
    let left_search = capture_query(&client.call(
        "search",
        rmcp::serde_json::json!({
            "snapshot_id": left_snapshot,
            "query": "left_staged_symbol",
            "kind": "function",
        }),
    ));
    let right_search = capture_query(&client.call(
        "search",
        rmcp::serde_json::json!({
            "snapshot_id": right_snapshot,
            "query": "right_staged_symbol",
            "kind": "function",
        }),
    ));
    let left_records = BTreeSet::from([GraphRecord::Symbol(
        "Function".into(),
        "left_staged_symbol".into(),
        "src/left.rs".into(),
        1,
    )]);
    let right_records = BTreeSet::from([GraphRecord::Symbol(
        "Function".into(),
        "right_staged_symbol".into(),
        "src/right.rs".into(),
        1,
    )]);
    assert_exact_query_records(&left_search.text, &left_records)
        .unwrap_or_else(|error| panic!("{error}: {}", left_search.text));
    assert_exact_query_records(&right_search.text, &right_records)
        .unwrap_or_else(|error| panic!("{error}: {}", right_search.text));
    let left_view = capture_query(&client.call(
        "view",
        rmcp::serde_json::json!({
            "snapshot_id": left_snapshot,
            "node_ref": left_search.text.split_whitespace().next().unwrap(),
            "depth": 1,
            "max_nodes": 30,
        }),
    ));
    let right_view = capture_query(&client.call(
        "view",
        rmcp::serde_json::json!({
            "snapshot_id": right_snapshot,
            "node_ref": right_search.text.split_whitespace().next().unwrap(),
            "depth": 1,
            "max_nodes": 30,
        }),
    ));
    let left_view_records = BTreeSet::from([
        GraphRecord::File("src/left.rs src/left.rs".into(), 1),
        GraphRecord::Symbol(
            "Function".into(),
            "left_staged_symbol".into(),
            "src/left.rs".into(),
            1,
        ),
    ]);
    let right_view_records = BTreeSet::from([
        GraphRecord::File("src/right.rs src/right.rs".into(), 1),
        GraphRecord::Symbol(
            "Function".into(),
            "right_staged_symbol".into(),
            "src/right.rs".into(),
            1,
        ),
    ]);
    assert_exact_query_records(&left_view.text, &left_view_records)
        .unwrap_or_else(|error| panic!("{error}: {}", left_view.text));
    assert_exact_query_records(&right_view.text, &right_view_records)
        .unwrap_or_else(|error| panic!("{error}: {}", right_view.text));
    let left_changes = capture_changes(&mut client, left_snapshot, 6, 50);
    let right_changes = capture_changes_in_order(
        &mut client,
        right_snapshot,
        6,
        50,
        [
            ("graph", "graph_next_cursor"),
            ("artifacts", "artifacts_next_cursor"),
            ("diff", "diff_next_cursor"),
            ("files", "files_next_cursor"),
            ("evidence", "evidence_next_cursor"),
        ],
    );
    assert_change_manifest(
        &left_changes,
        &["added source rust src/left.rs additions=1 deletions=0 layers=staged".into()],
    );
    assert_change_manifest(
        &right_changes,
        &["added source rust src/right.rs additions=1 deletions=0 layers=staged".into()],
    );
    assert_graph_records(
        &left_changes,
        &[("Function", "left_staged_symbol", "src/left.rs", 1)],
    );
    assert_graph_records(
        &right_changes,
        &[("Function", "right_staged_symbol", "src/right.rs", 1)],
    );
    for capture in std::iter::once(&left_search)
        .chain(std::iter::once(&left_view))
        .chain(left_changes.queries())
    {
        assert_eq!(capture.provenance, *left_provenance);
    }
    for capture in std::iter::once(&right_search)
        .chain(std::iter::once(&right_view))
        .chain(right_changes.queries())
    {
        assert_eq!(capture.provenance, *right_provenance);
    }
    assert_eq!(cached_snapshot(left), left_cache);
    assert_eq!(cached_snapshot(right), right_cache);
    assert_eq!(repository_state(&worktrees.linked), left_before);
    assert_eq!(repository_state(&second), right_before);
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
        "no changes reason=identical_commit_oids\n\
         content_complete_when_pages_exhausted=true\n\
         static_evidence_status=complete\n\
         dynamic_evidence_status=not-applicable\n"
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
fn long_indexing_keeps_status_inspect_and_existing_snapshot_queries_responsive() {
    let repository = Fixture::new();
    let controls = Fixture::new();
    fs::create_dir_all(repository.path.join("src")).unwrap();
    fs::write(
        repository.path.join("src/lib.rs"),
        "pub fn baseline_symbol() {}\n",
    )
    .unwrap();
    init_git(&repository.path);
    git(&repository.path, &["add", "--", "."]);
    git_commit(&repository.path, "baseline");
    fs::write(
        repository.path.join("src/lib.rs"),
        "pub fn baseline_symbol() {}\npub fn stable_snapshot_symbol() {}\n",
    )
    .unwrap();

    let blocker = GitBlocker::new(&controls.path, 1);
    let mut client = Client::start_unindexed_with(&repository.path, |command| {
        blocker.configure(command);
    });
    let completion = client.index_and_wait("boundary");
    let snapshot_id = completion["snapshot_id"].as_str().unwrap().to_owned();
    let provenance = completion["provenance"].clone();
    let old_search = capture_query(&client.search("stable_snapshot_symbol", Some("function")));
    let node_ref = old_search
        .text
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let old_view = capture_query(&client.view(&node_ref, 1, 30));
    let old_changes = capture_changes(&mut client, &snapshot_id, 6, 50);
    for capture in [&old_search, &old_view, &old_changes.initial] {
        assert_eq!(capture.provenance, provenance);
    }
    let old_cache = cached_snapshot(&completion);
    let cache_root =
        Path::new(completion["provenance"]["common_git_dir"].as_str().unwrap()).join("graphr/v6");
    let old_cache_tree = worktree_bytes(&cache_root, &cache_root);

    fs::write(
        repository.path.join("src/lib.rs"),
        "pub fn baseline_symbol() {}\npub fn stable_snapshot_symbol() {}\npub fn pending_cancelled_symbol() {}\n",
    )
    .unwrap();
    let intended_state = repository_state(&repository.path);
    blocker.block();
    let job_id = client.queue_index("boundary");
    blocker.wait_for_entries(1);

    let status = response_json(&client.call(
        "index_status",
        rmcp::serde_json::json!({ "job_id": &job_id }),
    ));
    assert_eq!(
        status["result"]["structuredContent"]["state"]["state"], "capturing",
        "{status}"
    );
    let inspection = response_json(&client.call(
        "inspect_root",
        rmcp::serde_json::json!({
            "worktree_root": &repository.path,
            "snapshot_id": &snapshot_id,
        }),
    ));
    assert_eq!(
        inspection["result"]["structuredContent"]["snapshot_matches_worktree"], false,
        "{inspection}"
    );
    let blocked_search = capture_query(&client.search("stable_snapshot_symbol", Some("function")));
    let blocked_view = capture_query(&client.view(&node_ref, 1, 30));
    let blocked_changes = capture_changes_in_order(
        &mut client,
        &snapshot_id,
        6,
        50,
        [
            ("artifacts", "artifacts_next_cursor"),
            ("graph", "graph_next_cursor"),
            ("files", "files_next_cursor"),
            ("diff", "diff_next_cursor"),
            ("evidence", "evidence_next_cursor"),
        ],
    );
    assert_eq!(blocked_search, old_search);
    assert_eq!(blocked_view, old_view);
    assert_eq!(blocked_changes, old_changes);

    let cancel = response_json(&client.call(
        "cancel_index",
        rmcp::serde_json::json!({ "job_id": &job_id }),
    ));
    assert_ne!(
        cancel["result"]["structuredContent"]["state"]["state"], "completed",
        "{cancel}"
    );
    blocker.release();
    client.wait_for_job(&job_id, "cancelled");

    assert_eq!(
        capture_query(&client.search("stable_snapshot_symbol", Some("function"))),
        old_search
    );
    assert_eq!(capture_query(&client.view(&node_ref, 1, 30)), old_view);
    assert_eq!(
        capture_changes(&mut client, &snapshot_id, 6, 50),
        old_changes
    );
    assert_eq!(cached_snapshot(&completion), old_cache);
    assert_eq!(worktree_bytes(&cache_root, &cache_root), old_cache_tree);
    assert_eq!(repository_state(&repository.path), intended_state);
    client.close();
}

#[test]
fn linked_feature_with_ten_commits_and_twelve_files_returns_replay_symbols() {
    let worktrees = linked_worktrees("ten-commit-divergence");
    let base_oid = git_output(&worktrees.main, &["rev-parse", "HEAD"]);
    let source = |prefix: &str| {
        (0..12)
            .map(|index| {
                format!(
                    "pub fn {prefix}_{index:02}() {{ let _ = \"{}\"; }}\n",
                    "generic_divergence_content_".repeat(8)
                )
            })
            .collect::<String>()
    };
    for commit in 0..9 {
        match commit {
            0 => {
                fs::write(
                    worktrees.linked.join("src/replay.rs"),
                    format!(
                        "pub fn replay_entry() {{ replay_step_00(); }}\n{}",
                        source("replay_step")
                    ),
                )
                .unwrap();
                fs::write(
                    worktrees.linked.join("linked.txt"),
                    (0..48)
                        .map(|index| {
                            format!(
                                "generic record {index:02} {}\n",
                                "divergence_value_".repeat(10)
                            )
                        })
                        .collect::<String>(),
                )
                .unwrap();
            }
            1..=7 => {
                let path = format!("src/change_{commit:02}_with_generic_divergence_evidence.rs");
                fs::write(
                    worktrees.linked.join(path),
                    source(&format!("change_{commit:02}")),
                )
                .unwrap();
            }
            8 => {
                for suffix in 8..=10 {
                    let path =
                        format!("src/change_{suffix:02}_with_generic_divergence_evidence.rs");
                    fs::write(
                        worktrees.linked.join(path),
                        source(&format!("change_{suffix:02}")),
                    )
                    .unwrap();
                }
            }
            _ => unreachable!(),
        }
        git(&worktrees.linked, &["add", "--", "."]);
        git_commit(&worktrees.linked, &format!("generic change {commit}"));
    }
    assert_eq!(
        git_output(
            &worktrees.linked,
            &["rev-list", "--count", &format!("{base_oid}..HEAD")],
        ),
        "10"
    );
    let expected_paths = [
        "linked.txt",
        "src/change_01_with_generic_divergence_evidence.rs",
        "src/change_02_with_generic_divergence_evidence.rs",
        "src/change_03_with_generic_divergence_evidence.rs",
        "src/change_04_with_generic_divergence_evidence.rs",
        "src/change_05_with_generic_divergence_evidence.rs",
        "src/change_06_with_generic_divergence_evidence.rs",
        "src/change_07_with_generic_divergence_evidence.rs",
        "src/change_08_with_generic_divergence_evidence.rs",
        "src/change_09_with_generic_divergence_evidence.rs",
        "src/change_10_with_generic_divergence_evidence.rs",
        "src/replay.rs",
    ];
    assert_eq!(
        git_output(
            &worktrees.linked,
            &["diff", "--name-only", &base_oid, "HEAD"],
        )
        .lines()
        .collect::<Vec<_>>(),
        expected_paths
    );
    let main_before = repository_state(&worktrees.main);
    let feature_before = repository_state(&worktrees.linked);

    let mut client = Client::start_unindexed_with(&worktrees.main, |command| {
        command.arg("--allow-root").arg(&worktrees.linked);
    });
    let main = client.index_target_and_wait(
        "HEAD",
        "HEAD",
        rmcp::serde_json::json!({ "kind": "commit" }),
    );
    let main_cache = cached_snapshot(&main);
    let queued = response_json(&client.call(
        "index",
        rmcp::serde_json::json!({
            "worktree_root": &worktrees.linked,
            "base": &base_oid,
            "head": "HEAD",
            "target": { "kind": "commit" },
            "dependency_mode": "boundary",
        }),
    ));
    let job_id = queued["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap();
    let completed = client.wait_for_job(job_id, "completed");
    let completion = &completed["result"]["structuredContent"]["state"]["completion"];
    let snapshot_id = completion["snapshot_id"].as_str().unwrap().to_owned();
    client.snapshot_id = Some(snapshot_id.clone());
    remember_graph(&worktrees.linked, completion);
    let provenance = &completion["provenance"];
    assert_eq!(provenance["commits_base_to_head"], 10);
    assert_eq!(provenance["changed_files"], 12);
    assert_eq!(provenance["target_state"]["kind"], "commit");
    assert_eq!(
        provenance["selected_layers"],
        rmcp::serde_json::json!(["committed"])
    );
    assert!(
        completion["stats"]["files_reused"].as_u64().unwrap() > 0,
        "unchanged Rust/Python sources were not reused: {completion}"
    );

    let first_changes = capture_changes(&mut client, &snapshot_id, 6, 50);
    assert!(!first_changes.initial.text.starts_with("no changes"));
    for label in [
        "files_next_cursor",
        "diff_next_cursor",
        "artifacts_next_cursor",
        "graph_next_cursor",
    ] {
        assert!(
            page_cursor(&first_changes.initial.text, label).is_some(),
            "{label} missing: {}",
            first_changes.initial.text
        );
    }
    assert_eq!(
        change_section_text(&first_changes, "files")
            .lines()
            .filter(|line| {
                line.starts_with("added ")
                    || line.starts_with("changed ")
                    || line.starts_with("deleted ")
                    || line.starts_with("renamed ")
            })
            .count(),
        12
    );
    let graph = parse_graph_records(&change_section_text(&first_changes, "graph")).unwrap();
    assert!(graph.contains(&GraphRecord::Symbol(
        "Function".into(),
        "replay_entry".into(),
        "src/replay.rs".into(),
        1,
    )));
    let search = capture_query(&client.search("replay_entry", Some("function")));
    let search_records = BTreeSet::from([GraphRecord::Symbol(
        "Function".into(),
        "replay_entry".into(),
        "src/replay.rs".into(),
        1,
    )]);
    assert_exact_query_records(&search.text, &search_records)
        .unwrap_or_else(|error| panic!("{error}: {}", search.text));
    let view = capture_query(&client.view(search.text.split_whitespace().next().unwrap(), 1, 30));
    let view_records = BTreeSet::from([
        GraphRecord::File("src/replay.rs src/replay.rs".into(), 1),
        GraphRecord::Symbol(
            "Function".into(),
            "replay_entry".into(),
            "src/replay.rs".into(),
            1,
        ),
        GraphRecord::Symbol(
            "Function".into(),
            "replay_step_00".into(),
            "src/replay.rs".into(),
            2,
        ),
    ]);
    assert_exact_query_records(&view.text, &view_records)
        .unwrap_or_else(|error| panic!("{error}: {}", view.text));
    for capture in std::iter::once(&search)
        .chain(std::iter::once(&view))
        .chain(first_changes.queries())
    {
        assert_eq!(capture.provenance, *provenance);
    }
    let first_cache = cached_snapshot(completion);
    assert_eq!(first_cache.manifest["provenance"], *provenance);
    assert_eq!(first_cache.files[&first_cache.graph_path].mode & 0o222, 0);

    let repeated = response_json(&client.call(
        "index",
        rmcp::serde_json::json!({
            "worktree_root": &worktrees.linked,
            "base": &base_oid,
            "head": "HEAD",
            "target": { "kind": "commit" },
            "dependency_mode": "boundary",
        }),
    ));
    let repeated_job = repeated["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap();
    let repeated = client.wait_for_job(repeated_job, "completed");
    let repeated = &repeated["result"]["structuredContent"]["state"]["completion"];
    assert_eq!(repeated["snapshot_id"], completion["snapshot_id"]);
    assert_eq!(repeated["graph_image_id"], completion["graph_image_id"]);
    assert_eq!(repeated["provenance"], *provenance);
    assert_eq!(
        repeated["stats"]["files_reused"], repeated["stats"]["files_total"],
        "exact graph was not fully reused: {repeated}"
    );
    let reused_changes = capture_changes_in_order(
        &mut client,
        &snapshot_id,
        6,
        50,
        [
            ("diff", "diff_next_cursor"),
            ("files", "files_next_cursor"),
            ("graph", "graph_next_cursor"),
            ("artifacts", "artifacts_next_cursor"),
            ("evidence", "evidence_next_cursor"),
        ],
    );
    assert_eq!(reused_changes, first_changes);
    assert_eq!(cached_snapshot(repeated), first_cache);
    assert_eq!(cached_snapshot(&main), main_cache);
    assert_eq!(repository_state(&worktrees.main), main_before);
    assert_eq!(repository_state(&worktrees.linked), feature_before);
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

struct GeneratedEvidenceFixture {
    fixture: Fixture,
    manifest: PathBuf,
}

#[derive(Clone, Copy)]
struct GeneratedAcceptanceOptions {
    decode_calls_predicate: bool,
    corrupt_output_digest: bool,
    include_test_name: bool,
    predicate_true_count: u64,
}

impl Default for GeneratedAcceptanceOptions {
    fn default() -> Self {
        Self {
            decode_calls_predicate: true,
            corrupt_output_digest: false,
            include_test_name: true,
            predicate_true_count: 1,
        }
    }
}

fn generated_acceptance_fixture(options: GeneratedAcceptanceOptions) -> GeneratedEvidenceFixture {
    const GENERATED_PATH: &str = "target/debug/build/graphr-fixture/out/message.rs";
    const COVERAGE_PATH: &str = "target/graphr/strict.json";
    const MANIFEST_PATH: &str = "target/graphr/evidence.json";

    let fixture = Fixture::new();
    for directory in [
        "proto",
        "src",
        "target/debug/build/graphr-fixture/out",
        "target/graphr",
    ] {
        fs::create_dir_all(fixture.path.join(directory)).unwrap();
    }
    fs::write(fixture.path.join(".gitignore"), "target/\n").unwrap();
    fs::write(
        fixture.path.join("proto/message.proto"),
        "message Message {\n  optional bool loose = 1;\n}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/generator.rs"),
        "pub fn emit(strict: bool) -> &'static str {\n    if strict { \"strict-code\" } else { \"loose-code\" }\n}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "mod predicate;\nmod generator;\n#[cfg(test)]\nmod tests;\npub mod message {\n    include!(concat!(env!(\"OUT_DIR\"), \"/message.rs\"));\n}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/predicate.rs"),
        "pub fn strict_predicate(value: u8) -> bool {\n    if value >= 0 { true } else { false }\n}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/tests.rs"),
        "#[test]\nfn strict_roundtrip() {\n    crate::message::encode(1);\n    crate::message::decode(1);\n}\n",
    )
    .unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "."]);
    git_commit(&fixture.path, "generated acceptance baseline");
    fs::write(
        fixture.path.join("proto/message.proto"),
        "message Message {\n  optional bool strict = 1;\n}\n",
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/predicate.rs"),
        "pub fn strict_predicate(value: u8) -> bool {\n    if value > 0 { true } else { false }\n}\n",
    )
    .unwrap();
    let source = index_repository(&fixture.path);

    let decode = if options.decode_calls_predicate {
        "pub fn decode(value: u8) -> bool { crate::predicate::strict_predicate(value) }\n"
    } else {
        "pub fn decode(value: u8) -> bool { value > 0 }\n"
    };
    let generated = format!(
        "pub fn encode(value: u8) -> bool {{ crate::predicate::strict_predicate(value) }}\n{decode}"
    );
    fs::write(fixture.path.join(GENERATED_PATH), generated).unwrap();
    let coverage = rmcp::serde_json::json!({
        "type": "llvm.coverage.json.export",
        "version": "2.0.1",
        "data": [{
            "functions": [
                {
                    "name": "encode",
                    "filenames": [GENERATED_PATH],
                    "regions": [[1, 1, 1, 82, 1, 0, 0, 0]]
                },
                {
                    "name": "decode",
                    "filenames": [GENERATED_PATH],
                    "regions": [[2, 1, 2, 82, 1, 0, 0, 0]]
                },
                {
                    "name": "strict_predicate",
                    "filenames": ["src/predicate.rs"],
                    "regions": [[1, 1, 3, 2, 1, 0, 0, 0]]
                }
            ],
            "files": [
                {"filename": GENERATED_PATH, "branches": []},
                {
                    "filename": "src/predicate.rs",
                    "branches": [[2, 5, 2, 18, options.predicate_true_count, 0, 0, 0, 4]]
                }
            ]
        }]
    });
    let coverage_bytes = rmcp::serde_json::to_vec(&coverage).unwrap();
    fs::write(fixture.path.join(COVERAGE_PATH), &coverage_bytes).unwrap();
    let generated_bytes = fs::read(fixture.path.join(GENERATED_PATH)).unwrap();
    let input_bytes = fs::read(fixture.path.join("proto/message.proto")).unwrap();
    let mut coverage_declaration = rmcp::serde_json::json!({
        "format": "llvm",
        "path": COVERAGE_PATH,
        "blake3": blake3::hash(&coverage_bytes).to_hex().to_string(),
        "run_label": "strict-run"
    });
    if options.include_test_name {
        coverage_declaration["test_name"] = "strict_roundtrip".into();
    }
    let output_digest = if options.corrupt_output_digest {
        "0".repeat(64)
    } else {
        blake3::hash(&generated_bytes).to_hex().to_string()
    };
    let manifest = fixture.path.join(MANIFEST_PATH);
    fs::write(
        &manifest,
        rmcp::serde_json::to_vec(&rmcp::serde_json::json!({
            "format_version": 1,
            "source_snapshot_id": source["snapshot_id"],
            "generated": [{
                "input": {
                    "path": "proto/message.proto",
                    "blake3": blake3::hash(&input_bytes).to_hex().to_string(),
                    "line_start": 2,
                    "line_end": 2
                },
                "generator": {
                    "path": "src/generator.rs",
                    "line_start": 2,
                    "line_end": 2
                },
                "output": {
                    "path": GENERATED_PATH,
                    "blake3": output_digest,
                    "line_start": 1,
                    "line_end": 2
                }
            }],
            "coverage": [coverage_declaration]
        }))
        .unwrap(),
    )
    .unwrap();
    GeneratedEvidenceFixture { fixture, manifest }
}

fn successful_generated_acceptance_index(
    evidence: &GeneratedEvidenceFixture,
) -> rmcp::serde_json::Value {
    let manifest = evidence
        .manifest
        .strip_prefix(&evidence.fixture.path)
        .unwrap()
        .to_str()
        .unwrap();
    let output = cli_index_with_evidence_request(
        &evidence.fixture.path,
        manifest,
        "HEAD",
        "HEAD",
        "worktree",
        true,
        "boundary",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    let completion = rmcp::serde_json::from_slice(output.stdout.trim_ascii()).unwrap();
    remember_graph(&evidence.fixture.path, &completion);
    completion
}

fn client_for_completion(path: &PathBuf, completion: &rmcp::serde_json::Value) -> Client {
    let mut client = Client::start_unindexed(path);
    client.snapshot_id = Some(completion["snapshot_id"].as_str().unwrap().into());
    let snapshot_id = client.snapshot_id().to_owned();
    let inspection = client.call(
        "inspect_root",
        rmcp::serde_json::json!({
            "worktree_root": path,
            "snapshot_id": snapshot_id,
        }),
    );
    assert!(!tool_failed(&inspection), "{inspection}");
    client
}

fn published_snapshots(path: &Path) -> BTreeSet<String> {
    let snapshots = path.join(".git/graphr/v6/snapshots");
    if !snapshots.exists() {
        return BTreeSet::new();
    }
    fs::read_dir(snapshots)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn generated_evidence_fixture() -> GeneratedEvidenceFixture {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "fn predicate() -> bool { true }\nfn generate() { include!(concat!(env!(\"OUT_DIR\"), \"/out.rs\")); }\n",
    )
    .unwrap();
    fs::write(fixture.path.join("schema.proto"), "message Input {}\n").unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "src/lib.rs", "schema.proto"]);
    git_commit(&fixture.path, "generated evidence source");
    let source = index_repository(&fixture.path);
    fs::create_dir(fixture.path.join("target")).unwrap();
    fs::write(
        fixture.path.join("target/out.rs"),
        "fn generated() -> bool { predicate() }\n",
    )
    .unwrap();
    let input = fs::read(fixture.path.join("schema.proto")).unwrap();
    let output = fs::read(fixture.path.join("target/out.rs")).unwrap();
    let manifest = fixture.path.join("evidence.json");
    fs::write(
        &manifest,
        format!(
            "{{\"format_version\":1,\"source_snapshot_id\":\"{}\",\"generated\":[{{\"input\":{{\"path\":\"schema.proto\",\"blake3\":\"{}\",\"line_start\":1,\"line_end\":1}},\"generator\":{{\"path\":\"src/lib.rs\",\"line_start\":2,\"line_end\":2}},\"output\":{{\"path\":\"target/out.rs\",\"blake3\":\"{}\",\"line_start\":1,\"line_end\":1}}}}],\"coverage\":[]}}",
            source["snapshot_id"].as_str().unwrap(),
            blake3::hash(&input).to_hex(),
            blake3::hash(&output).to_hex(),
        ),
    )
    .unwrap();
    GeneratedEvidenceFixture { fixture, manifest }
}

fn cli_index_with_evidence(path: &Path, manifest: &str) -> std::process::Output {
    cli_index_with_evidence_request(path, manifest, "HEAD", "HEAD", "worktree", true, "boundary")
}

#[allow(clippy::too_many_arguments)] // One call describes one complete public index request.
fn cli_index_with_evidence_request(
    path: &Path,
    manifest: &str,
    base: &str,
    head: &str,
    target: &str,
    include_untracked: bool,
    dependency_mode: &str,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphr"));
    command.args([
        "index",
        "--worktree-root",
        path.to_str().unwrap(),
        "--base",
        base,
        "--head",
        head,
        "--target",
        target,
    ]);
    if include_untracked {
        command.arg("--include-untracked");
    }
    command
        .args([
            "--dependency-mode",
            dependency_mode,
            "--evidence-manifest",
            manifest,
        ])
        .output()
        .unwrap()
}

fn successful_evidence_index(path: &Path) -> rmcp::serde_json::Value {
    let output = cli_index_with_evidence(path, "evidence.json");
    assert!(output.status.success(), "{:?}", output.stderr);
    rmcp::serde_json::from_slice(output.stdout.trim_ascii()).unwrap()
}

fn crate_graph_is_valid(path: &Path) {
    assert_eq!(
        Connection::open(path)
            .unwrap()
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

fn index_repository(path: &Path) -> rmcp::serde_json::Value {
    index_repository_request(path, "HEAD", "HEAD")
}

fn index_repository_request(path: &Path, base: &str, head: &str) -> rmcp::serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_graphr"))
        .args([
            "index",
            "--worktree-root",
            path.to_str().unwrap(),
            "--base",
            base,
            "--head",
            head,
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

fn assert_script_graph_matches_fresh(incremental: &Path, oracle: &Path) {
    index_repository(incremental);
    let oracle_cache = oracle.join(".git/graphr");
    if oracle_cache.exists() {
        fs::remove_dir_all(&oracle_cache).unwrap();
    }
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

#[derive(Debug, Eq, PartialEq)]
struct CachedFile {
    bytes: Vec<u8>,
    mode: u32,
    len: u64,
    inode: u64,
    modified: (i64, i64),
}

#[derive(Debug, Eq, PartialEq)]
struct CachedSnapshot {
    graph_path: PathBuf,
    files: BTreeMap<PathBuf, CachedFile>,
    manifest: rmcp::serde_json::Value,
}

fn cached_snapshot(completion: &rmcp::serde_json::Value) -> CachedSnapshot {
    let cache =
        Path::new(completion["provenance"]["common_git_dir"].as_str().unwrap()).join("graphr/v6");
    let snapshot_id = completion["snapshot_id"].as_str().unwrap();
    let graph_image_id = completion["graph_image_id"].as_str().unwrap();
    let manifest_path = cache.join("snapshots").join(format!("{snapshot_id}.json"));
    let manifest: rmcp::serde_json::Value =
        rmcp::serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let review_path = cache
        .join("reviews")
        .join(format!("{}.json", manifest["review_id"].as_str().unwrap()));
    let graph_path = cache.join("graphs").join(format!("{graph_image_id}.db"));
    let files = [&graph_path, &review_path, &manifest_path]
        .into_iter()
        .map(|path| {
            let metadata = fs::metadata(path).unwrap();
            (
                path.clone(),
                CachedFile {
                    bytes: fs::read(path).unwrap(),
                    mode: metadata.mode(),
                    len: metadata.len(),
                    inode: metadata.ino(),
                    modified: (metadata.mtime(), metadata.mtime_nsec()),
                },
            )
        })
        .collect();
    CachedSnapshot {
        graph_path,
        files,
        manifest,
    }
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

fn named_edge_kind_count(
    path: &Path,
    source_path: &str,
    source: &str,
    target_path: &str,
    target: &str,
    kind: &str,
) -> i64 {
    Connection::open(graph_path(path))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM edges edge
               JOIN nodes source ON source.id=edge.source_id
               JOIN files source_file ON source_file.id=source.file_id
               JOIN nodes target ON target.id=edge.target_id
               JOIN files target_file ON target_file.id=target.file_id
              WHERE source_file.path=?1 AND source.name=?2
                AND target_file.path=?3 AND target.name=?4 AND edge.kind=?5",
            [source_path, source, target_path, target, kind],
            |row| row.get(0),
        )
        .unwrap()
}

fn named_edge_support_count(
    path: &Path,
    source_path: &str,
    source: &str,
    target_path: &str,
    target: &str,
    kind: &str,
) -> i64 {
    Connection::open(graph_path(path))
        .unwrap()
        .query_row(
            "SELECT edge.support_count FROM edges edge
               JOIN nodes source ON source.id=edge.source_id
               JOIN files source_file ON source_file.id=source.file_id
               JOIN nodes target ON target.id=edge.target_id
               JOIN files target_file ON target_file.id=target.file_id
              WHERE source_file.path=?1 AND source.name=?2
                AND target_file.path=?3 AND target.name=?4 AND edge.kind=?5",
            [source_path, source, target_path, target, kind],
            |row| row.get(0),
        )
        .unwrap()
}

fn language_file_count(path: &Path, language: &str) -> i64 {
    Connection::open(graph_path(path))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM files WHERE language=?1",
            [language],
            |row| row.get(0),
        )
        .unwrap()
}

fn stored_file_language_and_context(path: &Path, source_path: &str) -> (String, String) {
    Connection::open(graph_path(path))
        .unwrap()
        .query_row(
            "SELECT language, parse_context FROM files WHERE path=?1",
            [source_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
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
    extra: Vec<PathBuf>,
}

impl LinkedWorktrees {
    fn add(&mut self, branch: &str) -> PathBuf {
        let path = self.root.join(branch);
        git(
            &self.main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                branch,
                path.to_str().unwrap(),
            ],
        );
        self.extra.push(path.clone());
        path
    }
}

impl Drop for LinkedWorktrees {
    fn drop(&mut self) {
        for linked in self.extra.iter().rev() {
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(linked)
                .current_dir(&self.main)
                .status();
        }
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
    let root = temp_root().join(format!(
        "graphr-e2e-linked-{label}-{}-{unique}",
        std::process::id()
    ));
    let main = root.join("main");
    let linked = root.join("linked");
    init_git_main(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("baseline.txt"), "baseline\n").unwrap();
    fs::write(
        main.join("src/shared.rs"),
        "pub fn unchanged_rust_symbol() {}\n",
    )
    .unwrap();
    fs::write(
        main.join("src/shared.py"),
        "def unchanged_python_symbol():\n    pass\n",
    )
    .unwrap();
    git(&main, &["add", "--", "."]);
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
    LinkedWorktrees {
        root,
        main,
        linked,
        extra: Vec::new(),
    }
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

struct GitBlocker {
    block: PathBuf,
    entered: PathBuf,
    claims: PathBuf,
    permits: usize,
    wrapper_path: std::ffi::OsString,
    real_git: PathBuf,
}

#[test]
fn blocked_git_markers_require_exact_distinct_roots() {
    let linked = PathBuf::from("/tmp/repository/linked");
    let linked_two = PathBuf::from("/tmp/repository/linked-two");
    let longer_marker = "--no-pager\n-c\ncore.fsmonitor=false\n-C\n/tmp/repository/linked-two\nrev-list\n--count\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa..bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n";
    let markers = [longer_marker.to_owned(), longer_marker.to_owned()];
    let expected = BTreeSet::from([linked.clone(), linked_two.clone()]);

    assert!(
        expected.iter().all(|root| markers
            .iter()
            .any(|marker| marker.contains(root.to_str().unwrap()))),
        "fixture no longer demonstrates the substring false positive"
    );
    assert!(
        assert_blocked_git_markers(&markers, &expected).is_err(),
        "duplicate longer-root markers were accepted"
    );
}

fn assert_blocked_git_markers(
    markers: &[String],
    expected: &BTreeSet<PathBuf>,
) -> Result<(), String> {
    if markers.len() != expected.len() {
        return Err("blocked Git marker count differs".into());
    }
    let actual = markers
        .iter()
        .map(|marker| parse_blocked_git_root(marker))
        .collect::<Result<BTreeSet<_>, _>>()?;
    (actual == *expected)
        .then_some(())
        .ok_or_else(|| format!("blocked Git roots differ: {actual:?}"))
}

fn parse_blocked_git_root(marker: &str) -> Result<PathBuf, String> {
    let args = marker.lines().collect::<Vec<_>>();
    let [
        "--no-pager",
        "-c",
        "core.fsmonitor=false",
        "-C",
        root,
        "rev-list",
        "--count",
        range,
    ] = args.as_slice()
    else {
        return Err(format!("unexpected blocked Git argv: {args:?}"));
    };
    let (base, head) = range
        .split_once("..")
        .ok_or_else(|| format!("unexpected blocked rev-list range: {range}"))?;
    if ![base, head].iter().all(|oid| {
        matches!(oid.len(), 40 | 64)
            && oid
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(format!("unexpected blocked rev-list range: {range}"));
    }
    let root = PathBuf::from(root);
    root.is_absolute()
        .then_some(root)
        .ok_or_else(|| "blocked Git root is not absolute".into())
}

impl GitBlocker {
    fn new(root: &Path, permits: usize) -> Self {
        let wrapper_dir = root.join("git-wrapper");
        let wrapper = wrapper_dir.join("git");
        let block = root.join("block-build");
        let entered = root.join("build-entered");
        let claims = root.join("build-claims");
        fs::create_dir(&wrapper_dir).unwrap();
        fs::create_dir(&entered).unwrap();
        fs::create_dir(&claims).unwrap();
        fs::write(
            &wrapper,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = rev-list ] && [ -e \"$GRAPHR_BLOCK_BUILD\" ]; then\n    slot=1\n    while [ \"$slot\" -le \"$GRAPHR_BLOCK_PERMITS\" ]; do\n      if mkdir \"$GRAPHR_BLOCK_CLAIMS/$slot\" 2>/dev/null; then\n        marker=\"$GRAPHR_BUILD_ENTERED.$$.tmp\"\n        printf '%s\\n' \"$@\" > \"$marker\"\n        mv \"$marker\" \"$GRAPHR_BUILD_ENTERED/$$\"\n        while [ -e \"$GRAPHR_BLOCK_BUILD\" ]; do :; done\n        break\n      fi\n      slot=$((slot + 1))\n    done\n  fi\ndone\nexec \"$GRAPHR_REAL_GIT\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        let wrapper_path = env::join_paths(
            std::iter::once(wrapper_dir)
                .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        Self {
            block,
            entered,
            claims,
            permits,
            wrapper_path,
            real_git: find_executable("git"),
        }
    }

    fn configure(&self, command: &mut Command) {
        command
            .env("PATH", &self.wrapper_path)
            .env("GRAPHR_REAL_GIT", &self.real_git)
            .env("GRAPHR_BLOCK_BUILD", &self.block)
            .env("GRAPHR_BUILD_ENTERED", &self.entered)
            .env("GRAPHR_BLOCK_CLAIMS", &self.claims)
            .env("GRAPHR_BLOCK_PERMITS", self.permits.to_string());
    }

    fn block(&self) {
        fs::write(&self.block, []).unwrap();
    }

    fn release(&self) {
        fs::remove_file(&self.block).unwrap();
    }

    fn wait_for_entries(&self, expected: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut entries = fs::read_dir(&self.entered)
                .unwrap()
                .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
                .collect::<Vec<_>>();
            if entries.len() == expected && entries.iter().all(|entry| !entry.is_empty()) {
                entries.sort();
                return entries;
            }
            assert!(
                Instant::now() < deadline,
                "expected {expected} blocked Git commands, found {}",
                entries.len()
            );
            thread::yield_now();
        }
    }
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

    fn index_target_and_wait(
        &mut self,
        base: &str,
        head: &str,
        target: rmcp::serde_json::Value,
    ) -> rmcp::serde_json::Value {
        let queued = response_json(&self.call(
            "index",
            rmcp::serde_json::json!({
                "worktree_root": self.repository,
                "base": base,
                "head": head,
                "target": target,
                "dependency_mode": "boundary",
            }),
        ));
        assert_eq!(
            queued["result"]["structuredContent"]["state"]["state"],
            "queued"
        );
        let job_id = queued["result"]["structuredContent"]["job_id"]
            .as_str()
            .unwrap();
        let value = self.wait_for_job(job_id, "completed");
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
        // The clock alone is not a unique name. macOS reports a coarser
        // realtime granularity than Linux, so two fixtures constructed on
        // parallel test threads can read the same nanosecond and collide on
        // `create_dir`. An ordinal makes the name unique within the process,
        // and the pid keeps concurrent `cargo test` runs apart.
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let ordinal = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = temp_root().join(format!(
            "graphr-e2e-{}-{unique}-{ordinal}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
}

/// macOS resolves `TMPDIR` under `/var`, a symlink to `/private/var`, and the
/// server reports canonical roots. Canonicalising here keeps a fixture root
/// comparable with what the server answers.
fn temp_root() -> PathBuf {
    let base = std::env::temp_dir();
    fs::canonicalize(&base).unwrap_or(base)
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
