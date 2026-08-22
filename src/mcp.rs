use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};

use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, ReadBuf};

use crate::git::DependencyMode;
use crate::index::Engine;
use crate::job::{JobRegistry, JobRequestSummary, JobStatus};
use crate::workspace::{
    ErrorCode, IndexRequest, OperationError, QueryOutput, RootInspection, SnapshotTarget,
    resolve_request,
};

const MCP_LINE_LIMIT: usize = 3 * 1024;

#[derive(Clone)]
struct Graphr {
    engine: Arc<Engine>,
    jobs: Arc<JobRegistry>,
}

#[derive(Clone, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct InspectRootParams {
    worktree_root: PathBuf,
    #[serde(default)]
    #[schemars(length(min = 64, max = 64))]
    snapshot_id: Option<String>,
}

#[derive(Clone, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct IndexParams {
    worktree_root: PathBuf,
    #[schemars(length(min = 1, max = 256))]
    base: String,
    #[schemars(length(min = 1, max = 256))]
    head: String,
    target: SnapshotTarget,
    dependency_mode: DependencyMode,
}

#[derive(Clone, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct JobParams {
    #[schemars(length(min = 1, max = 64))]
    job_id: String,
}

#[derive(Clone, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct SearchParams {
    #[schemars(length(min = 64, max = 64))]
    snapshot_id: String,
    query: String,
    #[serde(default)]
    kind: Option<SearchKind>,
    #[serde(default = "default_search_limit")]
    #[schemars(range(min = 1, max = 20))]
    limit: u32,
}

#[derive(Clone, Copy, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(crate = "rmcp::schemars")]
enum SearchKind {
    File,
    Type,
    Function,
    Test,
}

impl SearchKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Type => "type",
            Self::Function => "function",
            Self::Test => "test",
        }
    }
}

#[derive(Clone, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct ViewParams {
    #[schemars(length(min = 64, max = 64))]
    snapshot_id: String,
    #[schemars(length(min = 1, max = 116))]
    node_ref: String,
    #[serde(default = "default_depth")]
    #[schemars(range(min = 0, max = 6))]
    depth: u32,
    #[serde(default = "default_max_nodes")]
    #[schemars(range(min = 1, max = 50))]
    max_nodes: u32,
}

#[derive(Clone, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct ChangesParams {
    #[schemars(length(min = 64, max = 64))]
    snapshot_id: String,
    #[serde(default = "default_changes_depth")]
    #[schemars(range(min = 0, max = 6))]
    depth: u32,
    #[serde(default = "default_changes_max_nodes")]
    #[schemars(range(min = 1, max = 50))]
    max_nodes: u32,
    #[serde(default)]
    #[schemars(length(min = 1, max = 160))]
    cursor: Option<String>,
}

#[tool_router]
impl Graphr {
    #[tool(
        description = "Authorize and inspect the explicitly selected canonical Git worktree without indexing or fallback",
        output_schema = rmcp::handler::server::tool::schema_for_type::<RootInspection>()
    )]
    async fn inspect_root(
        &self,
        Parameters(params): Parameters<InspectRootParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let engine = self.engine.clone();
        Ok(structured_result(
            request_work(context, move |cancelled| {
                engine.inspect_root(
                    &params.worktree_root,
                    params.snapshot_id.as_deref(),
                    &cancelled,
                )
            })
            .await,
        ))
    }

    #[tool(
        description = "Queue an asynchronous immutable snapshot build for an explicit worktree, range, target, and dependency mode",
        output_schema = rmcp::handler::server::tool::schema_for_type::<JobStatus>()
    )]
    async fn index(
        &self,
        Parameters(params): Parameters<IndexParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let engine = self.engine.clone();
        let jobs = self.jobs.clone();
        Ok(structured_result(
            request_work(context, move |cancelled| {
                queue_index(engine, jobs, params, &cancelled)
            })
            .await,
        ))
    }

    #[tool(
        description = "Return the current state of one indexing job",
        output_schema = rmcp::handler::server::tool::schema_for_type::<JobStatus>()
    )]
    async fn index_status(
        &self,
        Parameters(params): Parameters<JobParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(structured_result(self.jobs.status(&params.job_id)))
    }

    #[tool(
        description = "Request cancellation of one indexing job",
        output_schema = rmcp::handler::server::tool::schema_for_type::<JobStatus>()
    )]
    async fn cancel_index(
        &self,
        Parameters(params): Parameters<JobParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(structured_result(self.jobs.cancel(&params.job_id)))
    }

    #[tool(description = "Find files, types, functions, and tests in one immutable snapshot")]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let kind = params.kind.map(SearchKind::as_str);
        Ok(query_result(self.engine.search(
            &params.snapshot_id,
            &params.query,
            kind,
            params.limit,
        )))
    }

    #[tool(description = "Show a compact neighborhood for a snapshot-bound node reference")]
    async fn view(
        &self,
        Parameters(params): Parameters<ViewParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(query_result(self.engine.view(
            &params.snapshot_id,
            &params.node_ref,
            params.depth,
            params.max_nodes,
        )))
    }

    #[tool(description = "Return an independently paged review for one explicit snapshot_id")]
    async fn changes(
        &self,
        Parameters(params): Parameters<ChangesParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let engine = self.engine.clone();
        Ok(query_result(
            request_work(context, move |cancelled| {
                engine.changes(
                    &params.snapshot_id,
                    params.depth,
                    params.max_nodes,
                    params.cursor.as_deref(),
                    &cancelled,
                )
            })
            .await,
        ))
    }
}

#[tool_handler]
impl ServerHandler for Graphr {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("graphr", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Graphr indexes Rust, Python, JavaScript/JSX, and TypeScript/TSX. Use inspect_root with the explicitly selected worktree_root. Use index with that worktree_root plus base, head, typed target (including include_untracked for a worktree target), and dependency_mode; verify the resolved root and OIDs. Poll index_status until completed and retain its snapshot_id; failed or cancelled is terminal. Call changes for that snapshot_id once without a cursor at depth 6 and max_nodes 50, then pass every files, diff, artifacts, and graph cursor verbatim with the same parameters until terminal completeness. Use search or view with the same snapshot_id only for named graph remediation. Stop on any structured root, job, snapshot, cursor, provenance, or completeness failure. Never fall back to another root, the default checkout, a live diff, or an older snapshot.",
            )
    }
}

pub async fn serve(engine: Arc<Engine>) -> Result<(), String> {
    let jobs = JobRegistry::new();
    let server = Graphr {
        engine,
        jobs: jobs.clone(),
    };
    let input = CappedInput {
        input: tokio::io::stdin(),
        line_bytes: 0,
    };
    let result = server
        .serve((input, tokio::io::stdout()))
        .await
        .map_err(|error| error.to_string())?
        .waiting()
        .await
        .map_err(|error| error.to_string());
    jobs.close();
    result.map(|_| ())
}

fn queue_index(
    engine: Arc<Engine>,
    jobs: Arc<JobRegistry>,
    params: IndexParams,
    cancelled: &AtomicBool,
) -> Result<JobStatus, OperationError> {
    let resolved = resolve_request(
        engine.roots(),
        IndexRequest {
            worktree_root: params.worktree_root,
            base_ref: params.base,
            head_ref: params.head,
            target: params.target,
            dependency_mode: params.dependency_mode,
        },
        cancelled,
    )?;
    let summary = JobRequestSummary {
        root: resolved.root.clone(),
        base_ref: resolved.base_ref.clone(),
        base_oid: resolved.base_oid.clone(),
        head_ref: resolved.head_ref.clone(),
        head_oid: resolved.head_oid.clone(),
        target: resolved.target.clone(),
        dependency_mode: resolved.dependency_mode,
    };
    let request_key = rmcp::serde_json::to_string(&summary).map_err(|error| {
        OperationError::new(
            ErrorCode::Internal,
            format!("cannot serialize indexing request: {error}"),
        )
    })?;
    let workspace_id = resolved.root.workspace_id.clone();
    jobs.start(
        workspace_id,
        request_key,
        summary,
        move |reporter, cancelled| {
            engine.build_snapshot(resolved, &cancelled, |progress| reporter.report(progress))
        },
    )
}

async fn request_work<T: Send + 'static>(
    context: RequestContext<RoleServer>,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<T, OperationError> + Send + 'static,
) -> Result<T, OperationError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let mut worker = tokio::task::spawn_blocking(move || work(worker_cancelled));
    tokio::select! {
        result = &mut worker => join_result(result),
        () = context.ct.cancelled() => {
            cancelled.store(true, Ordering::Relaxed);
            join_result(worker.await)
        }
    }
}

fn join_result<T>(
    result: Result<Result<T, OperationError>, tokio::task::JoinError>,
) -> Result<T, OperationError> {
    result.map_err(|error| {
        OperationError::new(ErrorCode::Internal, format!("worker failed: {error}"))
    })?
}

fn structured_result<T: Serialize>(result: Result<T, OperationError>) -> CallToolResult {
    match result {
        Ok(value) => match rmcp::serde_json::to_value(value) {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => operation_error(OperationError::new(
                ErrorCode::Internal,
                format!("cannot serialize tool result: {error}"),
            )),
        },
        Err(error) => operation_error(error),
    }
}

fn query_result(result: Result<QueryOutput, OperationError>) -> CallToolResult {
    match result {
        Ok(output) => {
            let mut result = CallToolResult::success(vec![ContentBlock::text(output.text)]);
            let mut structured = rmcp::serde_json::Map::new();
            structured.insert(
                "provenance".into(),
                rmcp::serde_json::to_value(output.provenance)
                    .expect("provenance is JSON serializable"),
            );
            if let Some(reason) = output.no_change_reason {
                structured.insert(
                    "no_change_reason".into(),
                    rmcp::serde_json::to_value(reason).expect("reason is JSON serializable"),
                );
            }
            result.structured_content = Some(structured.into());
            result
        }
        Err(error) => operation_error(error),
    }
}

fn operation_error(error: OperationError) -> CallToolResult {
    CallToolResult::structured_error(
        rmcp::serde_json::to_value(error).expect("operation error is JSON serializable"),
    )
}

struct CappedInput {
    input: tokio::io::Stdin,
    line_bytes: usize,
}

impl AsyncRead for CappedInput {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.input).poll_read(context, buffer);
        if matches!(&result, Poll::Ready(Ok(())))
            && let Err(error) = track_input_lines(&mut self.line_bytes, &buffer.filled()[before..])
        {
            return Poll::Ready(Err(error));
        }
        result
    }
}

fn track_input_lines(line_bytes: &mut usize, input: &[u8]) -> std::io::Result<()> {
    for byte in input {
        if *byte == b'\n' {
            *line_bytes = 0;
        } else if *line_bytes >= MCP_LINE_LIMIT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP input line exceeds 3 KiB",
            ));
        } else {
            *line_bytes += 1;
        }
    }
    Ok(())
}

const fn default_search_limit() -> u32 {
    8
}

const fn default_depth() -> u32 {
    1
}

const fn default_max_nodes() -> u32 {
    30
}

const fn default_changes_depth() -> u32 {
    1
}

const fn default_changes_max_nodes() -> u32 {
    50
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use crate::job::JobState;
    use crate::workspace::{AllowedRoots, Provenance};

    use super::*;

    #[test]
    fn tool_schemas_require_explicit_root_or_snapshot_context() {
        let tools = Graphr::tool_router().list_all();
        let by_name = |name: &str| tools.iter().find(|tool| tool.name == name).unwrap();

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "cancel_index",
                "changes",
                "index",
                "index_status",
                "inspect_root",
                "search",
                "view",
            ])
        );
        for (name, required) in [
            ("inspect_root", &["worktree_root"][..]),
            (
                "index",
                &["worktree_root", "base", "head", "target", "dependency_mode"][..],
            ),
            ("index_status", &["job_id"][..]),
            ("cancel_index", &["job_id"][..]),
            ("search", &["snapshot_id", "query"][..]),
            ("view", &["snapshot_id", "node_ref"][..]),
            ("changes", &["snapshot_id"][..]),
        ] {
            let tool = by_name(name);
            for property in required {
                assert!(
                    tool.input_schema["required"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|value| value == property),
                    "{name} does not require {property}: {:?}",
                    tool.input_schema
                );
            }
        }
        let index = by_name("index");
        let reference = index.input_schema["properties"]["target"]["$ref"]
            .as_str()
            .unwrap();
        let definition = reference.rsplit('/').next().unwrap();
        assert_eq!(
            index.input_schema["$defs"][definition]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        for name in ["inspect_root", "index", "index_status", "cancel_index"] {
            assert!(by_name(name).output_schema.is_some(), "{name}");
        }
    }

    #[test]
    fn tool_and_server_guidance_requires_the_explicit_snapshot_workflow() {
        let root = repository("guidance");
        let jobs = JobRegistry::new();
        let server = Graphr {
            engine: Arc::new(Engine::new(Arc::new(
                AllowedRoots::new(vec![root.clone()]).unwrap(),
            ))),
            jobs: jobs.clone(),
        };
        let instructions = server.get_info().instructions.unwrap();

        assert!(instructions.contains("Rust, Python, JavaScript/JSX, and TypeScript/TSX"));

        for required in [
            "worktree_root",
            "base",
            "head",
            "target",
            "include_untracked",
            "dependency_mode",
            "index_status",
            "completed",
            "snapshot_id",
            "once without a cursor",
            "files, diff, artifacts, and graph",
            "verbatim",
            "search or view",
            "Stop on any structured root, job, snapshot, cursor, provenance, or completeness failure",
            "Never fall back",
        ] {
            assert!(
                instructions.contains(required),
                "missing {required:?}: {instructions}"
            );
        }

        let tools = Graphr::tool_router().list_all();
        let description = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap()
                .description
                .as_deref()
                .unwrap()
        };
        assert!(description("inspect_root").contains("explicitly selected"));
        assert!(description("index").contains("asynchronous"));
        assert!(description("changes").contains("snapshot_id"));

        jobs.close();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn index_returns_a_queued_job_before_graph_build() {
        let root = repository("queued");
        let engine = Arc::new(Engine::new(Arc::new(
            AllowedRoots::new(vec![root.clone()]).unwrap(),
        )));
        let jobs = JobRegistry::new();
        let status = queue_index(
            engine,
            jobs.clone(),
            IndexParams {
                worktree_root: root.clone(),
                base: "HEAD".into(),
                head: "HEAD".into(),
                target: SnapshotTarget::Commit,
                dependency_mode: DependencyMode::Boundary,
            },
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(status.state, JobState::Queued);
        assert_eq!(
            status.request.root.worktree_root,
            fs::canonicalize(&root).unwrap()
        );
        assert_eq!(status.request.base_oid, status.request.head_oid);
        jobs.close();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_snapshot_never_falls_back_to_an_allowed_root() {
        let root = repository("unknown-snapshot");
        let engine = Engine::new(Arc::new(AllowedRoots::new(vec![root.clone()]).unwrap()));
        let error = engine
            .search(&"a".repeat(64), "source", None, 8)
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::SnapshotNotFound);
        assert_eq!(error.details["snapshot_id"], "a".repeat(64));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn structured_errors_retain_codes_and_details() {
        let result = operation_error(
            OperationError::new(ErrorCode::RootDisallowed, "root is not allowed")
                .with_detail("root", "/tmp/outside"),
        );

        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "root_disallowed"
        );
        assert_eq!(
            result.structured_content.as_ref().unwrap()["details"]["root"],
            "/tmp/outside"
        );
    }

    #[test]
    fn large_query_results_attach_provenance_without_duplicating_text() {
        let text = "record\n".repeat(200);
        let result = query_result(Ok(QueryOutput {
            text: text.clone(),
            provenance: provenance(),
            no_change_reason: None,
        }));

        assert_eq!(result.content[0].as_text().unwrap().text, text);
        let structured = result.structured_content.unwrap();
        assert!(structured.get("text").is_none());
        assert_eq!(structured["provenance"]["snapshot_id"], "c".repeat(64));
    }

    #[test]
    fn caps_mcp_input_lines() {
        let mut line_bytes = 0;
        assert!(track_input_lines(&mut line_bytes, &vec![b'a'; MCP_LINE_LIMIT]).is_ok());
        assert!(track_input_lines(&mut line_bytes, b"a").is_err());
        assert!(track_input_lines(&mut line_bytes, b"\nok\n").is_ok());
        assert_eq!(line_bytes, 0);
    }

    fn repository(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "graphr-mcp-{label}-{}-{}",
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
        fs::write(root.join("src/lib.rs"), "pub fn source() {}\n").unwrap();
        git(&root, &["add", "--", "."]);
        git(&root, &["commit", "--quiet", "-m", "baseline"]);
        root
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
    }

    fn provenance() -> Provenance {
        Provenance {
            repository_id: "a".repeat(64),
            workspace_id: "b".repeat(64),
            snapshot_id: "c".repeat(64),
            common_git_dir: "/tmp/repo/.git".into(),
            git_dir: "/tmp/repo/.git".into(),
            repository_root: "/tmp/repo".into(),
            worktree_root: "/tmp/repo".into(),
            branch: Some("main".into()),
            base_ref: "main".into(),
            base_oid: "d".repeat(40),
            head_ref: "HEAD".into(),
            head_oid: "d".repeat(40),
            target_state: SnapshotTarget::Commit,
            selected_layers: Vec::new(),
            dirty_digest: "e".repeat(64),
            commits_base_to_head: 0,
            changed_files: 0,
            index_generation: 1,
        }
    }
}
