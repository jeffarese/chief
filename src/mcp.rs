//! Coordinator MCP server over stdio (ARCHITECTURE §7), built on `rmcp`. Only adapts: the tools
//! live in `tools.rs`. stdout carries the protocol, so logs go to stderr.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde_json::Value;

use crate::claude::{Claude, RESOLVE_TIMEOUT};
use crate::config::ProjectConfig;
use crate::tools::{
    self, ListThreadsArgs, MoveThreadArgs, NameArgs, ReadReportArgs, ResolveThreadArgs,
    StartThreadArgs, ToolCtx,
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
        description = "Start a thread: a background Claude Code session that owns one task on its own branch, in the Claude account with the most room. Returns its name and address; then subscribe with SendMessage(to=address, notify_when_idle=true) and no message, as `next` says."
    )]
    async fn start_thread(
        &self,
        Parameters(args): Parameters<StartThreadArgs>,
        request: RequestContext<RoleServer>,
    ) -> CallToolResult {
        // rmcp cancels the token on the client's notifications/cancelled, and on shutdown, but
        // the blocking tool runs on: it checks the token itself right before the spawn.
        let token = request.ct;
        self.call(move |ctx| tools::start_thread_unless(ctx, &args, &|| token.is_cancelled()))
            .await
    }

    #[tool(
        description = "List this project's threads with their board group, waiting reason, the subagents and background commands still running, whether asleep, branch, PR, account, the address to message them at, and a preview of the last report."
    )]
    async fn list_threads(&self, Parameters(args): Parameters<ListThreadsArgs>) -> CallToolResult {
        self.call(move |ctx| tools::list_threads(ctx, &args)).await
    }

    #[tool(
        description = "Read a thread's full latest report (idle notices are cut short), and what it still runs: a thread with subagents or background commands running isn't finished. history returns more reports, newest first."
    )]
    async fn read_report(&self, Parameters(args): Parameters<ReadReportArgs>) -> CallToolResult {
        self.call(move |ctx| tools::read_report(ctx, &args)).await
    }

    #[tool(
        description = "Bring an asleep thread back (respawn its process) so it can receive messages. Returns whether it is alive and its address, which changes for a thread in another account."
    )]
    async fn wake_thread(&self, Parameters(args): Parameters<NameArgs>) -> CallToolResult {
        self.call(move |ctx| tools::wake_thread(ctx, &args)).await
    }

    #[tool(
        description = "Stop a thread's process: its current turn ends unfinished and it sends no report for it. Its conversation, worktree and branch stay; wake_thread brings it back. Only when the user asks, or a thread is clearly off track and messaging it hasn't helped."
    )]
    async fn stop_thread(&self, Parameters(args): Parameters<NameArgs>) -> CallToolResult {
        self.call(move |ctx| tools::stop_thread(ctx, &args)).await
    }

    #[tool(
        description = "Move a thread to another Claude account now: agp stops it and resumes the same conversation in the same worktree there, starting a new turn that says it moved. agp does this by itself when a thread's account hits a usage limit; use this only when the user asks, or when list_threads shows a thread limited that agp didn't move. Returns its account and the address to subscribe to again."
    )]
    async fn move_thread(&self, Parameters(args): Parameters<MoveThreadArgs>) -> CallToolResult {
        self.call(move |ctx| tools::move_thread(ctx, &args)).await
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
    // The project's account, whatever the server's environment names (§3).
    let claude = Claude::for_account(config.claude_config_dir.clone());
    let trust_config = claude.trust_config();
    let agp = crate::agp::Agp::from_env();
    let ctx = ToolCtx {
        conn: &conn,
        config: &config,
        claude: &claude,
        agp: &agp,
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
