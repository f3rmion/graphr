use std::pin::Pin;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};

use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::Mutex as TokioMutex;

use crate::git::DependencyMode;
use crate::index::Project;

type ToolResult = Result<String, String>;
const MCP_LINE_LIMIT: usize = 3 * 1024;

#[derive(Clone)]
struct Graphr {
    project: Arc<Project>,
    jobs: Arc<TokioMutex<()>>,
    cancellation: Arc<JobCancellation>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SearchParams {
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

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ViewParams {
    node_ref: String,
    #[serde(default = "default_depth")]
    #[schemars(range(min = 0, max = 6))]
    depth: u32,
    #[serde(default = "default_max_nodes")]
    #[schemars(range(min = 1, max = 50))]
    max_nodes: u32,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ChangesParams {
    #[serde(default = "default_changes_base")]
    #[schemars(length(min = 1, max = 256))]
    base: String,
    #[serde(default = "default_changes_depth")]
    #[schemars(range(min = 0, max = 6))]
    depth: u32,
    /// Maximum graph records per response page; cursors continue the snapshot.
    #[serde(default = "default_changes_max_nodes")]
    #[schemars(range(min = 1, max = 50))]
    max_nodes: u32,
    #[serde(default)]
    dependency_mode: DependencyModeParam,
    #[serde(default)]
    #[schemars(length(min = 1, max = 128))]
    cursor: Option<String>,
}

#[derive(Clone, Copy, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(crate = "rmcp::schemars")]
enum DependencyModeParam {
    #[default]
    Boundary,
    Full,
}

impl DependencyModeParam {
    const fn value(self) -> DependencyMode {
        match self {
            Self::Boundary => DependencyMode::Boundary,
            Self::Full => DependencyMode::Full,
        }
    }
}

impl Graphr {
    async fn exclusive_job(
        &self,
        context: RequestContext<RoleServer>,
        busy: &'static str,
        budget: usize,
        work: impl FnOnce(Arc<Project>, Arc<AtomicBool>) -> ToolResult + Send + 'static,
    ) -> ToolResult {
        let _guard = self
            .jobs
            .clone()
            .try_lock_owned()
            .map_err(|_| busy.to_owned())?;
        let cancelled = self.cancellation.begin();
        let _cancel_on_drop = CancelOnDrop {
            state: self.cancellation.clone(),
            flag: cancelled.clone(),
        };
        let project = self.project.clone();
        let worker_cancelled = cancelled.clone();
        let mut worker = tokio::task::spawn_blocking(move || work(project, worker_cancelled));
        let result = tokio::select! {
            result = &mut worker => result.map_err(|error| format!("worker failed: {error}"))?,
            () = context.ct.cancelled() => {
                cancelled.store(true, Ordering::Relaxed);
                worker.await.map_err(|error| format!("worker failed: {error}"))?
            }
        };
        error_budget(result, budget)
    }
}

#[tool_router]
impl Graphr {
    #[tool(description = "Refresh the code graph for this repository")]
    async fn index(&self, context: RequestContext<RoleServer>) -> ToolResult {
        self.exclusive_job(context, "index busy", 256, |project, cancelled| {
            project.index_cancelled(false, cancelled)
        })
        .await
    }

    #[tool(
        description = "Find files, types, functions, and tests",
        input_schema = rmcp::handler::server::common::schema_for_input::<SearchParams>()
            .expect("valid search schema")
    )]
    async fn search(&self, Parameters(raw): Parameters<rmcp::serde_json::Value>) -> ToolResult {
        let params: SearchParams = rmcp::serde_json::from_value(raw)
            .map_err(|_| "invalid search parameters".to_owned())?;
        validate_search(&params)?;
        let project = self.project.clone();
        let kind = params.kind.map(SearchKind::as_str);
        error_budget(
            blocking(move || project.search(&params.query, kind, params.limit)).await,
            1536,
        )
    }

    #[tool(
        description = "Show a compact neighborhood for a node_ref up to 6 graph hops",
        input_schema = rmcp::handler::server::common::schema_for_input::<ViewParams>()
            .expect("valid view schema")
    )]
    async fn view(&self, Parameters(raw): Parameters<rmcp::serde_json::Value>) -> ToolResult {
        let params: ViewParams =
            rmcp::serde_json::from_value(raw).map_err(|_| "invalid view parameters".to_owned())?;
        if params.depth > 6 {
            return Err("depth must be in 0..=6".into());
        }
        if !(1..=50).contains(&params.max_nodes) {
            return Err("max_nodes must be in 1..=50".into());
        }
        let project = self.project.clone();
        error_budget(
            blocking(move || project.view(&params.node_ref, params.depth, params.max_nodes)).await,
            4096,
        )
    }

    #[tool(
        description = "Return an initial or cursor-selected bounded review page: changed-file manifest, Rust/Python source diff, non-source artifact text diffs and semantics, explained risk scores, affected static call paths, and graph impact up to 6 hops. Call once without a cursor. Cursors are standalone name=value lines: split on the first =, return the complete value verbatim with the original arguments, and exhaust files_next_cursor, diff_next_cursor, artifacts_next_cursor, and graph_next_cursor from one immutable snapshot. analysis_complete is analyzer-local; review_complete=false means do not conclude: follow present cursors, then report incomplete unless all are absent and review_complete_when_pages_exhausted=true. This server is bound to its startup repository and working tree; for an unchecked-out A..B range, use a separate server at B with A as base. max_nodes bounds graph records per page, never snapshot coverage. Explicit artifact omissions keep coverage incomplete. Cargo-vendored changes collapse to package boundaries by default; use dependency_mode=full for internals. Flow discovery traces CALLS up to 15 hops",
        input_schema = rmcp::handler::server::common::schema_for_input::<ChangesParams>()
            .expect("valid changes schema")
    )]
    async fn changes(
        &self,
        Parameters(raw): Parameters<rmcp::serde_json::Value>,
        context: RequestContext<RoleServer>,
    ) -> ToolResult {
        let params: ChangesParams = rmcp::serde_json::from_value(raw)
            .map_err(|_| "invalid changes parameters".to_owned())?;
        validate_changes(&params)?;
        self.exclusive_job(context, "changes busy", 8192, move |project, cancelled| {
            project.changes_cancelled(
                &params.base,
                params.depth,
                params.max_nodes,
                params.dependency_mode.value(),
                params.cursor.as_deref(),
                cancelled,
            )
        })
        .await
    }
}

#[tool_handler]
impl ServerHandler for Graphr {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("graphr", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "For reviews, call index once only after Rust or Python source edits made in this session; otherwise use the startup index. Then call changes once without a cursor using the review base, dependency_mode, and a depth from 0 through 6. Cursors are standalone name=value lines: split on the first = and return the complete value verbatim with the same arguments. Exhaust every files_next_cursor, diff_next_cursor, artifacts_next_cursor, and graph_next_cursor from one immutable snapshot, including artifact text; do not start another cursorless changes call. analysis_complete is analyzer-local; review_complete=false means do not conclude: follow present cursors, then report incomplete unless all are absent and review_complete_when_pages_exhausted=true. This server is bound to its startup repository and working tree; for an unchecked-out A..B range, use a separate server at B with A as base. max_nodes bounds graph records per page, never snapshot coverage. dependency_mode defaults to boundary, which accounts for .cargo/vendor changes as package boundaries without analyzing internals; use full only when dependency internals are review scope. Changes captures Rust/Python source diffs plus bounded generic text diffs and Markdown/TSV semantics; complete artifact coverage does not add indexed source languages. Binary, oversized, unsafe, non-regular, type-changed, unmerged, and other explicit omissions keep review_complete_when_pages_exhausted false and must not be read as fallback. Risk output includes direction, component scores, rationale, test_path_confidence=heuristic, test_path_provenance=resolved-static-call-graph, and affected static call paths; flow discovery follows CALLS up to 15 hops, so these are possible source paths, not runtime call stacks. Follow explicit graph coverage remediation with targeted search or view. A stale or failing cursor or any explicit omission means coverage is incomplete.",
            )
    }
}

pub async fn serve(project: Project) -> Result<(), String> {
    let cancellation = Arc::new(JobCancellation::default());
    let server = Graphr {
        project: Arc::new(project),
        jobs: Arc::new(TokioMutex::new(())),
        cancellation: cancellation.clone(),
    };
    let input = CancelOnEof {
        input: tokio::io::stdin(),
        cancellation,
        line_bytes: 0,
    };
    server
        .serve((input, tokio::io::stdout()))
        .await
        .map_err(|error| error.to_string())?
        .waiting()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn blocking(call: impl FnOnce() -> ToolResult + Send + 'static) -> ToolResult {
    // ponytail: read-only jobs finish after client cancellation; add SQLite
    // InterruptHandle wiring if measured cancellation latency becomes material.
    tokio::task::spawn_blocking(call)
        .await
        .map_err(|error| format!("worker failed: {error}"))?
}

fn error_budget(result: ToolResult, limit: usize) -> ToolResult {
    result.map_err(|mut error| {
        if error.len() > limit {
            let mut end = limit;
            while !error.is_char_boundary(end) {
                end -= 1;
            }
            error.truncate(end);
        }
        error
    })
}

#[derive(Default)]
struct JobCancellation {
    closed: AtomicBool,
    active: StdMutex<Option<Arc<AtomicBool>>>,
}

impl JobCancellation {
    fn begin(&self) -> Arc<AtomicBool> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let flag = Arc::new(AtomicBool::new(self.closed.load(Ordering::Acquire)));
        *active = Some(flag.clone());
        flag
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(flag) = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            flag.store(true, Ordering::Relaxed);
        }
    }

    fn finish(&self, flag: &Arc<AtomicBool>) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, flag))
        {
            *active = None;
        }
    }
}

struct CancelOnDrop {
    state: Arc<JobCancellation>,
    flag: Arc<AtomicBool>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::Relaxed);
        self.state.finish(&self.flag);
    }
}

struct CancelOnEof {
    input: tokio::io::Stdin,
    cancellation: Arc<JobCancellation>,
    line_bytes: usize,
}

impl AsyncRead for CancelOnEof {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let had_capacity = buffer.remaining() > 0;
        let result = Pin::new(&mut self.input).poll_read(context, buffer);
        if matches!(&result, Poll::Ready(Ok(())))
            && let Err(error) = track_input_lines(&mut self.line_bytes, &buffer.filled()[before..])
        {
            self.cancellation.close();
            return Poll::Ready(Err(error));
        }
        if matches!(&result, Poll::Ready(Err(_)))
            || matches!(&result, Poll::Ready(Ok(())) if had_capacity && buffer.filled().len() == before)
        {
            self.cancellation.close();
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

fn validate_search(params: &SearchParams) -> Result<(), String> {
    if params.query.trim().is_empty() || params.query.len() > 256 {
        return Err("query must contain 1..=256 UTF-8 bytes".into());
    }
    if !(1..=20).contains(&params.limit) {
        return Err("limit must be in 1..=20".into());
    }
    Ok(())
}

fn validate_changes(params: &ChangesParams) -> Result<(), String> {
    if params.base.trim().is_empty()
        || params.base.len() > 256
        || params.base.trim_start().starts_with('-')
        || params.base.chars().any(char::is_control)
    {
        return Err("invalid changes base".into());
    }
    if params.depth > 6 {
        return Err("depth must be in 0..=6".into());
    }
    if !(1..=50).contains(&params.max_nodes) {
        return Err("max_nodes must be in 1..=50".into());
    }
    if params.cursor.as_ref().is_some_and(|cursor| {
        cursor.is_empty()
            || cursor.len() > 128
            || !cursor.is_ascii()
            || cursor.chars().any(char::is_control)
    }) {
        return Err("invalid changes cursor".into());
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

fn default_changes_base() -> String {
    "HEAD".into()
}

const fn default_changes_depth() -> u32 {
    1
}

const fn default_changes_max_nodes() -> u32 {
    50
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_search_boundaries() {
        let valid = SearchParams {
            query: "dispatch".into(),
            kind: Some(SearchKind::Function),
            limit: 20,
        };
        assert!(validate_search(&valid).is_ok());
        assert!(validate_search(&SearchParams { limit: 0, ..valid }).is_err());
        assert!(
            rmcp::serde_json::from_value::<SearchParams>(
                rmcp::serde_json::json!({ "query": "dispatch", "kind": "method" })
            )
            .is_err()
        );
    }

    #[test]
    fn schemas_publish_tool_bounds() {
        let search = rmcp::serde_json::to_value(rmcp::schemars::schema_for!(SearchParams)).unwrap();
        assert_eq!(search["properties"]["limit"]["minimum"], 1);
        assert_eq!(search["properties"]["limit"]["maximum"], 20);
        assert_eq!(
            search["$defs"]["SearchKind"]["enum"],
            rmcp::serde_json::json!(["file", "type", "function", "test"])
        );

        let view = rmcp::serde_json::to_value(rmcp::schemars::schema_for!(ViewParams)).unwrap();
        assert_eq!(view["properties"]["depth"]["minimum"], 0);
        assert_eq!(view["properties"]["depth"]["maximum"], 6);
        assert_eq!(view["properties"]["max_nodes"]["minimum"], 1);
        assert_eq!(view["properties"]["max_nodes"]["maximum"], 50);

        let changes =
            rmcp::serde_json::to_value(rmcp::schemars::schema_for!(ChangesParams)).unwrap();
        assert_eq!(changes["properties"]["base"]["minLength"], 1);
        assert_eq!(changes["properties"]["base"]["maxLength"], 256);
        assert_eq!(changes["properties"]["depth"]["minimum"], 0);
        assert_eq!(changes["properties"]["depth"]["maximum"], 6);
        assert_eq!(changes["properties"]["max_nodes"]["minimum"], 1);
        assert_eq!(changes["properties"]["max_nodes"]["maximum"], 50);
        assert_eq!(
            changes["$defs"]["DependencyModeParam"]["enum"],
            rmcp::serde_json::json!(["boundary", "full"])
        );
        assert_eq!(changes["properties"]["cursor"]["minLength"], 1);
        assert_eq!(changes["properties"]["cursor"]["maxLength"], 128);
    }

    #[test]
    fn validates_changes_defaults_and_boundaries() {
        let defaults: ChangesParams =
            rmcp::serde_json::from_value(rmcp::serde_json::json!({})).unwrap();
        assert_eq!(defaults.base, "HEAD");
        assert_eq!(defaults.depth, 1);
        assert_eq!(defaults.max_nodes, 50);
        assert!(matches!(
            defaults.dependency_mode,
            DependencyModeParam::Boundary
        ));
        assert_eq!(defaults.cursor, None);
        assert!(validate_changes(&defaults).is_ok());
        assert!(
            rmcp::serde_json::from_value::<ChangesParams>(
                rmcp::serde_json::json!({ "dependency_mode": "transitive" })
            )
            .is_err()
        );
        assert!(
            validate_changes(&ChangesParams {
                base: "HEAD".into(),
                depth: 6,
                max_nodes: 50,
                dependency_mode: DependencyModeParam::Boundary,
                cursor: Some("a".repeat(128)),
            })
            .is_ok()
        );

        for invalid in [
            ChangesParams {
                base: " ".into(),
                depth: 2,
                max_nodes: 50,
                dependency_mode: DependencyModeParam::Boundary,
                cursor: None,
            },
            ChangesParams {
                base: "-HEAD".into(),
                depth: 2,
                max_nodes: 50,
                dependency_mode: DependencyModeParam::Boundary,
                cursor: None,
            },
            ChangesParams {
                base: "HEAD".into(),
                depth: 7,
                max_nodes: 50,
                dependency_mode: DependencyModeParam::Boundary,
                cursor: None,
            },
            ChangesParams {
                base: "HEAD".into(),
                depth: 2,
                max_nodes: 0,
                dependency_mode: DependencyModeParam::Boundary,
                cursor: None,
            },
        ] {
            assert!(validate_changes(&invalid).is_err());
        }
        assert!(
            validate_changes(&ChangesParams {
                base: "a".repeat(257),
                depth: 2,
                max_nodes: 50,
                dependency_mode: DependencyModeParam::Boundary,
                cursor: None,
            })
            .is_err()
        );
        for cursor in ["".into(), "a".repeat(129), "é".into(), "a\n".into()] {
            assert!(
                validate_changes(&ChangesParams {
                    base: "HEAD".into(),
                    depth: 2,
                    max_nodes: 50,
                    dependency_mode: DependencyModeParam::Boundary,
                    cursor: Some(cursor),
                })
                .is_err()
            );
        }
    }

    #[test]
    fn tool_errors_obey_utf8_budgets() {
        let error = error_budget(Err("é".repeat(200)), 255).unwrap_err();
        assert!(error.len() <= 255);
        assert!(error.is_char_boundary(error.len()));
    }

    #[test]
    fn caps_mcp_input_lines() {
        let mut line_bytes = 0;
        assert!(track_input_lines(&mut line_bytes, &vec![b'a'; MCP_LINE_LIMIT]).is_ok());
        assert!(track_input_lines(&mut line_bytes, b"a").is_err());
        assert!(track_input_lines(&mut line_bytes, b"\nok\n").is_ok());
        assert_eq!(line_bytes, 0);
    }
}
