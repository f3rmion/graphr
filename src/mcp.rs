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

use crate::index::Project;

type ToolResult = Result<String, String>;
const MCP_LINE_LIMIT: usize = 3 * 1024;

#[derive(Clone)]
struct Grapher {
    project: Arc<Project>,
    indexing: Arc<TokioMutex<()>>,
    cancellation: Arc<IndexCancellation>,
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
    #[schemars(range(min = 0, max = 3))]
    depth: u32,
    #[serde(default = "default_max_nodes")]
    #[schemars(range(min = 1, max = 50))]
    max_nodes: u32,
}

#[tool_router]
impl Grapher {
    #[tool(description = "Refresh the Rust code graph for this repository")]
    async fn index(&self, context: RequestContext<RoleServer>) -> ToolResult {
        let _guard = self
            .indexing
            .clone()
            .try_lock_owned()
            .map_err(|_| "index busy".to_owned())?;
        let cancelled = self.cancellation.begin();
        let _cancel_on_drop = CancelOnDrop {
            state: self.cancellation.clone(),
            flag: cancelled.clone(),
        };
        let project = self.project.clone();
        let worker_cancelled = cancelled.clone();
        let mut worker =
            tokio::task::spawn_blocking(move || project.index_cancelled(false, worker_cancelled));
        let result = tokio::select! {
            result = &mut worker => result.map_err(|error| format!("worker failed: {error}"))?,
            () = context.ct.cancelled() => {
                cancelled.store(true, Ordering::Relaxed);
                worker.await.map_err(|error| format!("worker failed: {error}"))?
            }
        };
        error_budget(result, 256)
    }

    #[tool(
        description = "Find Rust files, types, functions, and tests",
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
        description = "Show a compact bounded neighborhood for a node_ref",
        input_schema = rmcp::handler::server::common::schema_for_input::<ViewParams>()
            .expect("valid view schema")
    )]
    async fn view(&self, Parameters(raw): Parameters<rmcp::serde_json::Value>) -> ToolResult {
        let params: ViewParams =
            rmcp::serde_json::from_value(raw).map_err(|_| "invalid view parameters".to_owned())?;
        if params.depth > 3 {
            return Err("depth must be in 0..=3".into());
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
}

#[tool_handler]
impl ServerHandler for Grapher {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("grapher", env!("CARGO_PKG_VERSION")))
            .with_instructions("Use search to get a node_ref, then view its graph.")
    }
}

pub async fn serve(project: Project) -> Result<(), String> {
    let cancellation = Arc::new(IndexCancellation::default());
    let server = Grapher {
        project: Arc::new(project),
        indexing: Arc::new(TokioMutex::new(())),
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
struct IndexCancellation {
    closed: AtomicBool,
    active: StdMutex<Option<Arc<AtomicBool>>>,
}

impl IndexCancellation {
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
    state: Arc<IndexCancellation>,
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
    cancellation: Arc<IndexCancellation>,
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

const fn default_search_limit() -> u32 {
    8
}

const fn default_depth() -> u32 {
    1
}

const fn default_max_nodes() -> u32 {
    30
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
        assert_eq!(view["properties"]["depth"]["maximum"], 3);
        assert_eq!(view["properties"]["max_nodes"]["minimum"], 1);
        assert_eq!(view["properties"]["max_nodes"]["maximum"], 50);
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
