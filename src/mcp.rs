//! The MCP server (stdio).

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{Json, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::finder::{Finder, Match, SessionSummary};

/// Default number of matches returned by `find_herdr_session`.
pub const DEFAULT_LIMIT: usize = 5;

const INSTRUCTIONS: &str = "Finds the user's running Herdr coding-agent sessions \
(Claude Code, Codex, ...; in the Herdr TUI or in herdr-core terminals shown by the Herdr app) by what they are about and switches the user's view to one. \
When the user asks to go to, switch to, open, jump to or be taken to the Herdr \
window/pane/tab/session/agent where some topic was discussed or worked on, call \
find_herdr_session with that topic, pick the best match (ask if it's ambiguous), then \
call focus_herdr_pane with its pane_id. Use list_herdr_sessions when keywords don't find it.";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindArgs {
    /// What the session is about, in the user's words, e.g. "refactoring the config parser".
    pub description: String,
    /// Maximum number of matches to return (default 5).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Also consider the calling agent's own pane (default false).
    #[serde(default)]
    pub include_current: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FocusArgs {
    /// The pane_id of a session from find_herdr_session or list_herdr_sessions
    /// (a Herdr TUI pane id, or a herdr-core terminal address like `t7`).
    pub pane_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FindResult {
    /// Best matches first. Empty when nothing matched; try list_herdr_sessions.
    pub matches: Vec<Match>,
    /// How many agent sessions were searched.
    pub sessions_searched: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListResult {
    pub sessions: Vec<SessionSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FocusResult {
    /// The pane now shown to the user.
    pub focused: String,
}

/// The `herdr-find` MCP server.
#[derive(Clone)]
pub struct Server {
    finder: Arc<Finder>,
    tool_router: ToolRouter<Self>,
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result.map_err(|e| format!("{e:#}")),
        Err(e) => Err(format!("internal error: {e}")),
    }
}

#[tool_router(router = tool_router)]
impl Server {
    pub fn new(finder: Finder) -> Self {
        Self {
            finder: Arc::new(finder),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "find_herdr_session",
        description = "Find the user's running Herdr agent session (Claude Code, Codex, ...) by what it is about. \
Use when the user asks to take them to, switch to, open, jump to or find the Herdr window, pane, tab or session \
where they were chatting about / working on something, e.g. \"take me to the herdr window where we're \
refactoring the config parser\". Searches pane titles, conversation summaries, workspace/tab names, working \
directories and the sessions' transcripts, and returns the best matches (pane_id, runtime, title, workspace, tab, cwd, \
score, matching snippet, first and recent prompts). The calling agent's own pane is excluded. Then call \
focus_herdr_pane with the chosen pane_id; if the top matches are close or unconvincing, ask the user or use \
list_herdr_sessions."
    )]
    async fn find_herdr_session(
        &self,
        Parameters(args): Parameters<FindArgs>,
    ) -> Result<Json<FindResult>, String> {
        let finder = self.finder.clone();
        blocking(move || {
            let limit = args.limit.map_or(DEFAULT_LIMIT, |n| n.max(1) as usize);
            let (matches, sessions_searched) = finder.find(
                &args.description,
                limit,
                args.include_current.unwrap_or(false),
            )?;
            Ok(Json(FindResult {
                matches,
                sessions_searched,
                warnings: finder.warnings(),
            }))
        })
        .await
    }

    #[tool(
        name = "list_herdr_sessions",
        description = "List every running Herdr agent session (Claude Code, Codex, ...; Herdr TUI panes and \
herdr-core terminals) with its pane_id, runtime, status, \
title, conversation summary, workspace, tab, working directory, first prompt and latest prompts. Use it to pick \
the session the user means when find_herdr_session's keyword search doesn't find it, or when the user asks what \
agents are running in Herdr."
    )]
    async fn list_herdr_sessions(&self) -> Result<Json<ListResult>, String> {
        let finder = self.finder.clone();
        blocking(move || {
            Ok(Json(ListResult {
                sessions: finder.list()?,
                warnings: finder.warnings(),
            }))
        })
        .await
    }

    #[tool(
        name = "focus_herdr_pane",
        description = "Switch the user's Herdr view to an agent pane, including its workspace and tab (for a \
herdr-core terminal, the Herdr app shows it): this is how you \"take the user to\" a session. Pass a pane_id from find_herdr_session or list_herdr_sessions."
    )]
    async fn focus_herdr_pane(
        &self,
        Parameters(args): Parameters<FocusArgs>,
    ) -> Result<Json<FocusResult>, String> {
        let finder = self.finder.clone();
        blocking(move || {
            finder.focus(&args.pane_id)?;
            Ok(Json(FocusResult {
                focused: args.pane_id,
            }))
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("herdr-find", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serve MCP on stdin/stdout until the client disconnects.
pub async fn serve(finder: Finder) -> anyhow::Result<()> {
    let service = Server::new(finder).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
