//! Coordinator MCP server over stdio (ARCHITECTURE §7), built on `rmcp`. Only adapts: the tools
//! live in `tools.rs`. stdout carries the protocol, so logs go to stderr.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde_json::Value;

use crate::claude::{self, Claude, RESOLVE_TIMEOUT};
use crate::config::ProjectConfig;
use crate::tools::{
    self, ListThreadsArgs, NameArgs, ReadReportArgs, ResolveThreadArgs, StartThreadArgs, ToolCtx,
};
use crate::{db, paths};

const WAKE_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn run(project: &str) -> Result<()> {
    let dir = paths::project_dir(project);
    if !dir.join("project.toml").exists() {
        bail!("no chief project {project:?} (looked in {})", dir.display());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = ChiefMcp::new(project)
            .serve(rmcp::transport::stdio())
            .await
            .context("starting the MCP server")?;
        service.waiting().await.context("running the MCP server")?;
        Ok(())
    })
}

#[derive(Clone)]
pub struct ChiefMcp {
    project: Arc<str>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ChiefMcp {
    pub fn new(project: &str) -> Self {
        ChiefMcp {
            project: project.into(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Start a thread: a background Claude Code session that owns one task on its own branch. Returns its name; then subscribe with SendMessage(to=name, notify_when_idle=true) and no message."
    )]
    async fn start_thread(&self, Parameters(args): Parameters<StartThreadArgs>) -> CallToolResult {
        self.call(move |ctx| tools::start_thread(ctx, &args)).await
    }

    #[tool(
        description = "List this project's threads with their board group, waiting reason, whether asleep, branch, PR and a preview of the last report."
    )]
    async fn list_threads(&self, Parameters(args): Parameters<ListThreadsArgs>) -> CallToolResult {
        self.call(move |ctx| tools::list_threads(ctx, &args)).await
    }

    #[tool(
        description = "Read a thread's full latest report (idle notices are cut short). history returns more reports, newest first."
    )]
    async fn read_report(&self, Parameters(args): Parameters<ReadReportArgs>) -> CallToolResult {
        self.call(move |ctx| tools::read_report(ctx, &args)).await
    }

    #[tool(
        description = "Bring an asleep thread back (respawn its process) so it can receive messages. Returns whether it is alive."
    )]
    async fn wake_thread(&self, Parameters(args): Parameters<NameArgs>) -> CallToolResult {
        self.call(move |ctx| tools::wake_thread(ctx, &args)).await
    }

    #[tool(
        description = "Mark a thread resolved (done), or reopen it with reopen=true. Resolved threads can't be messaged."
    )]
    async fn resolve_thread(
        &self,
        Parameters(args): Parameters<ResolveThreadArgs>,
    ) -> CallToolResult {
        self.call(move |ctx| tools::resolve_thread(ctx, &args))
            .await
    }
}

impl ChiefMcp {
    /// Runs a tool on the blocking pool (it shells out and sleeps) with a fresh context: the
    /// settings may have changed since the last call.
    async fn call<F>(&self, f: F) -> CallToolResult
    where
        F: FnOnce(&ToolCtx) -> Result<Value> + Send + 'static,
    {
        let project = self.project.clone();
        let result = tokio::task::spawn_blocking(move || with_ctx(&project, f)).await;
        let result = match result {
            Ok(r) => r,
            Err(e) => Err(anyhow::anyhow!("the tool crashed: {e}")),
        };
        match result {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Err(e) => {
                let message = format!("{e:#}");
                eprintln!("chief mcp: {message}");
                CallToolResult::error(vec![ContentBlock::text(message)])
            }
        }
    }
}

fn with_ctx<T>(project: &str, f: impl FnOnce(&ToolCtx) -> Result<T>) -> Result<T> {
    let dir = crate::project::project_dir(project);
    let config = ProjectConfig::load(&dir.join("project.toml"))?;
    let conn = db::open(&paths::db())?;
    let claude = Claude::from_env();
    let trust_config = claude::claude_config_path();
    let ctx = ToolCtx {
        conn: &conn,
        config: &config,
        claude: &claude,
        project_dir: &dir,
        trust_config: &trust_config,
        now: db::now_ms,
        resolve_timeout: RESOLVE_TIMEOUT,
        wake_timeout: WAKE_TIMEOUT,
        poll_interval: POLL_INTERVAL,
    };
    f(&ctx)
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ChiefMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("chief", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Tools for this chief project's threads. Message threads with SendMessage; \
                 use these to start, list, read, wake and resolve them.",
            )
    }
}
