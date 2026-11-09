//! Coordinator MCP server over stdio (ARCHITECTURE §7), built on `rmcp`. Only adapts: the tools
//! live in `tools.rs`. stdout carries the protocol, so logs go to stderr.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde_json::Value;

use crate::paths;
use crate::routines::{self, Draft, RoutineNameArgs, UpdateRoutineArgs};
use crate::tools::{
    self, ListThreadsArgs, MoveThreadArgs, NameArgs, ReadReportArgs, ResolveThreadArgs,
    StartThreadArgs, ToolCtx,
};

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

    #[tool(
        description = "List this project's routines: prompts that run on a schedule, each run a fresh thread. Shows each one's schedule in words, whether it is paused, when it next runs, and the thread and outcome of its last run."
    )]
    async fn list_routines(&self) -> CallToolResult {
        self.call(routines::list_tool).await
    }

    #[tool(
        description = "Create a routine: a prompt chief runs on a schedule, each run a fresh thread that works without anyone watching. Use it when the user wants something done regularly (\"every weekday at 9, triage new issues\"). The prompt is each run's whole brief. Returns the routine with its next run."
    )]
    async fn create_routine(&self, Parameters(args): Parameters<Draft>) -> CallToolResult {
        self.call(move |ctx| routines::create_tool(ctx, &args))
            .await
    }

    #[tool(
        description = "Change a routine: its title, prompt, schedule, repo, model or effort, whether you are told when a run finishes, or pause and resume it with enabled. Only the fields you pass change."
    )]
    async fn update_routine(
        &self,
        Parameters(args): Parameters<UpdateRoutineArgs>,
    ) -> CallToolResult {
        self.call(move |ctx| routines::update_tool(ctx, &args))
            .await
    }

    #[tool(
        description = "Delete a routine, only when the user asks. Its past runs' threads and reports stay. To stop it for a while, pause it with update_routine(enabled=false) instead."
    )]
    async fn delete_routine(
        &self,
        Parameters(args): Parameters<RoutineNameArgs>,
    ) -> CallToolResult {
        self.call(move |ctx| routines::delete_tool(ctx, &args))
            .await
    }

    #[tool(
        description = "Run a routine now, outside its schedule: starts its thread and returns its name. Its schedule doesn't move. Nothing starts while its last run is still working."
    )]
    async fn run_routine(&self, Parameters(args): Parameters<RoutineNameArgs>) -> CallToolResult {
        self.call(move |ctx| routines::run_tool(ctx, &args)).await
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
        let result = tokio::task::spawn_blocking(move || tools::with_project(&project, f)).await;
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

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ChiefMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("chief", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Tools for this chief project's threads and routines. Message threads with \
                 SendMessage; use these to start, list, read, wake and resolve them, and to \
                 schedule work that repeats.",
            )
    }
}
