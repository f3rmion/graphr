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

    let indexed = Command::new(env!("CARGO_BIN_EXE_grapher"))
        .args(["index", fixture.path.to_str().unwrap(), "--rebuild"])
        .output()
        .unwrap();
    assert!(indexed.status.success(), "{:?}", indexed.stderr);
    assert_eq!(
        Connection::open(fixture.path.join(".git/grapher/index.db"))
            .unwrap()
            .query_row("SELECT count(*) FROM edges WHERE kind='CALLS'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
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
            "mod caller;\nmod ext;\nmod target;\n",
        )
        .unwrap();
        fs::write(root.join("src/caller.rs"), CALLER).unwrap();
        fs::write(
            root.join("src/ext.rs"),
            "use crate::target::Widget;\nimpl Widget { pub fn ping(&self) {} }\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("crates/app/src")).unwrap();
        fs::write(root.join("crates/app/src/worker.rs"), "pub fn work() {}\n").unwrap();
        init_git(root);
        assert!(
            Command::new("git")
                .args([
                    "-C",
                    root.to_str().unwrap(),
                    "add",
                    "--",
                    "src",
                    "crates/app/src/worker.rs",
                ])
                .status()
                .unwrap()
                .success()
        );
    }

    index_repository(&incremental.path, true);
    index_repository(&oracle.path, true);
    assert_eq!(
        semantic_graph(&incremental.path),
        semantic_graph(&oracle.path)
    );
    assert_resolution(&incremental.path, None, "file");

    let generation = database_generation(&incremental.path.join(".git/grapher/index.db"));
    assert_incremental_matches_rebuild(&incremental.path, &oracle.path, 0);
    assert_eq!(
        database_generation(&incremental.path.join(".git/grapher/index.db")),
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
fn concurrent_initial_indexes_serialize() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(fixture.path.join("src/lib.rs"), "pub fn run() {}\n").unwrap();
    init_git(&fixture.path);

    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_grapher"));
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

    let database = fixture.path.join(".git/grapher/index.db");
    let concurrent = normalized_graph(&database);
    let rebuilt = Command::new(env!("CARGO_BIN_EXE_grapher"))
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
    assert!(
        Command::new("git")
            .args([
                "-C",
                fixture.path.to_str().unwrap(),
                "add",
                "--",
                "src/pipe.rs"
            ])
            .status()
            .unwrap()
            .success()
    );
    fs::remove_file(&fifo).unwrap();
    let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let indexed = Command::new(env!("CARGO_BIN_EXE_grapher"))
        .args(["index", fixture.path.to_str().unwrap(), "--rebuild"])
        .env("GIT_LITERAL_PATHSPECS", "1")
        .output()
        .unwrap();
    assert!(indexed.status.success(), "{:?}", indexed.stderr);
    assert_eq!(
        String::from_utf8(indexed.stdout).unwrap(),
        "indexed generation=1 changed=2 skipped=4\n"
    );

    let database = fixture.path.join(".git/grapher/index.db");
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

    let rejected = Command::new(env!("CARGO_BIN_EXE_grapher"))
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

    let rebuilt = Command::new(env!("CARGO_BIN_EXE_grapher"))
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
    let healed = Command::new(env!("CARGO_BIN_EXE_grapher"))
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
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"grapher-test","version":"0"}}}"#,
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
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"grapher-test","version":"0"}}}"#,
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
        "#!/bin/sh\nif [ -e \"$GRAPHER_BLOCK_GIT\" ]; then\n  printf '%s\\n' \"$$\" > \"$GRAPHER_GIT_ENTERED\"\n  exec sleep 600\nfi\n\"$GRAPHER_REAL_GIT\" \"$@\"\nstatus=$?\nprintf 'done\\n' >> \"$GRAPHER_GIT_FINISHED\"\nexit \"$status\"\n",
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
            .env("GRAPHER_REAL_GIT", &real_git)
            .env("GRAPHER_BLOCK_GIT", &block_git)
            .env("GRAPHER_GIT_ENTERED", &git_entered)
            .env("GRAPHER_GIT_FINISHED", &git_finished);
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
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"grapher-test","version":"0"}}}"#,
    );
    assert!(initialized.contains("\"id\":1"));
    client.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    let tools = client.request(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    for name in ["index", "search", "view"] {
        assert!(tools.contains(&format!("\"name\":\"{name}\"")));
    }
    for invalid in [
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search","arguments":{"query":"dispatch","kind":"method"}}}"#,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"search","arguments":{"query":" ","limit":0}}}"#,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"view","arguments":{"node_ref":"bad","depth":4}}}"#,
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
    let node_ref = response_text(&search)
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();

    let view = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":41,"method":"tools/call","params":{{"name":"view","arguments":{{"node_ref":"{node_ref}","depth":2,"max_nodes":30}}}}}}"#
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
        let node_ref = response_text(&search).split_whitespace().next().unwrap();
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
        r#"{"jsonrpc":"2.0","id":47,"method":"tools/call","params":{"name":"index","arguments":{}}}"#,
    );
    assert!(busy.contains("index busy"), "{busy}");
    thread::sleep(Duration::from_millis(50));
    let closed = Instant::now();
    client.close();
    assert!(closed.elapsed() < Duration::from_secs(1));
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(database_generation(&database), generation_before_eof);
}

fn index_repository(path: &Path, rebuild: bool) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_grapher"));
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

fn assert_resolution(path: &Path, support: Option<i64>, parent_kind: &str) {
    let connection = Connection::open(path.join(".git/grapher/index.db")).unwrap();
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
    let connection = Connection::open(path.join(".git/grapher/index.db")).unwrap();
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
                            target.qualified_name, ref_key.rank, ref_key.key
                        )
                   FROM refs reference
                   JOIN nodes source ON source.id=reference.source_id
                   LEFT JOIN nodes target ON target.id=reference.resolved_target_id
                   JOIN ref_keys ref_key ON ref_key.ref_id=reference.id
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

fn init_git(path: &PathBuf) {
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
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

fn response_text(response: &str) -> &str {
    let marker = "\"text\":\"";
    let text = response
        .split_once(marker)
        .map(|(_, text)| text)
        .expect("text tool result");
    text.split_once('"').map_or(text, |(text, _)| text)
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_grapher"));
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
        let path =
            std::env::temp_dir().join(format!("grapher-e2e-{}-{unique}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
