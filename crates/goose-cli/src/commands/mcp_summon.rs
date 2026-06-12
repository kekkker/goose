use anyhow::Result;
use goose::agents::mcp_client::McpClientTrait;
use goose::agents::platform_extensions::summon::SummonClient;
use goose::agents::platform_extensions::PlatformExtensionContext;
use goose::agents::ToolCallContext;
use goose::config::GooseMode;
use goose::session::session_manager::SessionType;
use goose::session::SessionManager;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorCode, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

/// Standalone MCP server that exposes the `delegate` and `load` tools from
/// the summon platform extension over stdio. Register it in Claude Code's
/// mcpServers config so that the delegate tool is available in every session,
/// including claude-acp sessions where goose's ACP provider cannot forward
/// platform extensions.
pub struct SummonMcpServer {
    client: Arc<SummonClient>,
    session_id: String,
}

impl SummonMcpServer {
    pub async fn new() -> Result<Self> {
        let session_manager = Arc::new(SessionManager::instance());

        let working_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));

        let session = session_manager
            .create_session(
                working_dir,
                "goose-mcp-summon".to_string(),
                SessionType::Hidden,
                GooseMode::Auto,
            )
            .await?;

        let session_id = session.id.clone();

        let context = PlatformExtensionContext {
            extension_manager: None,
            session_manager,
            session: None,
            use_login_shell_path: false,
        };

        let client = Arc::new(SummonClient::new(context)?);

        Ok(Self { client, session_id })
    }
}

/// Cached nesting flag: captures the inherited env value ONCE at server start
/// before `run_summon_mcp_server` mutates the environment.
///
/// `None` means the lock has not been initialised yet (only possible in unit
/// tests that call `is_nested()` without going through
/// `run_summon_mcp_server`); in that case we fall back to reading the live env.
static NESTED: OnceLock<bool> = OnceLock::new();

/// Decode a raw env-var value into a nesting flag.
///
/// `None`  → var absent → not nested
/// `Some("")` or `Some("0")` → explicitly disabled → not nested
/// anything else → nested
pub fn nested_from(value: Option<&str>) -> bool {
    match value {
        None | Some("") | Some("0") => false,
        Some(_) => true,
    }
}

/// Return `true` when this process is a nested summon instance (spawned by a
/// subagent that already has its own summon MCP server).
///
/// Workers inherit the main session's MCP config and therefore start their own
/// `goose mcp summon` process, which would offer `delegate` and allow unlimited
/// recursion.  Setting `GOOSE_SUMMON_NESTED=1` in the child environment prevents
/// that by limiting the nested server to `load` only.
///
/// The check uses `NESTED` — a value captured once by `run_summon_mcp_server`
/// BEFORE it sets `GOOSE_SUMMON_NESTED=1`.  This prevents the top-level server
/// from seeing its own marker and hiding `delegate` from itself.
pub fn is_nested() -> bool {
    *NESTED.get_or_init(|| nested_from(std::env::var("GOOSE_SUMMON_NESTED").ok().as_deref()))
}

impl ServerHandler for SummonMcpServer {
    fn get_info(&self) -> ServerInfo {
        let instructions = if is_nested() {
            "Goose summon extension (nested): delegation is unavailable at this depth — \
             use `load` to read available recipes/agents and do the work directly."
                .to_string()
        } else {
            "Goose summon extension: use `delegate` to run subagent tasks and \
             `load` to list/read available recipes and agents."
                .to_string()
        };
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "goose-summon",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        if is_nested() {
            // Nested context: only expose `load`, no `delegate`.
            let all = self
                .client
                .list_tools(&self.session_id, None, CancellationToken::new())
                .await
                .map_err(|e| McpError::new(ErrorCode::INTERNAL_ERROR, e.to_string(), None))?;
            let tools = all
                .tools
                .into_iter()
                .filter(|t| t.name.as_ref() == "load")
                .collect();
            return Ok(ListToolsResult {
                tools,
                next_cursor: None,
                meta: None,
            });
        }
        self.client
            .list_tools(&self.session_id, None, CancellationToken::new())
            .await
            .map_err(|e| McpError::new(ErrorCode::INTERNAL_ERROR, e.to_string(), None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let ctx = ToolCallContext::new(self.session_id.clone(), None, None);
        self.client
            .call_tool(
                &ctx,
                &request.name,
                request.arguments,
                CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::new(ErrorCode::INTERNAL_ERROR, e.to_string(), None))
    }
}

pub async fn run_summon_mcp_server() -> Result<()> {
    use rmcp::{transport::stdio, ServiceExt};

    // Capture whether THIS process was spawned as a nested instance BEFORE we
    // set the env var — the OnceLock must be initialised from the inherited
    // value, not from the value we are about to write.
    let _ = is_nested();

    // Now mark the environment so that any ACP child processes spawned from
    // this server (subagents) inherit the flag and their own `goose mcp summon`
    // instance will refuse to expose `delegate`, breaking the recursion cycle.
    // SAFETY: this is the server's main task; no other threads read env vars
    // concurrently at this point.
    #[allow(deprecated)]
    std::env::set_var("GOOSE_SUMMON_NESTED", "1");

    let server = SummonMcpServer::new().await?;
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("summon mcp server error: {:?}", e))?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── pure decision-logic tests (no env mutation, safe under parallel test threads) ──

    #[test]
    fn nested_from_none_means_not_nested() {
        assert!(!nested_from(None), "absent var must not be nested");
    }

    #[test]
    fn nested_from_empty_means_not_nested() {
        assert!(!nested_from(Some("")), "empty value must not be nested");
    }

    #[test]
    fn nested_from_zero_means_not_nested() {
        assert!(!nested_from(Some("0")), "value '0' must not be nested");
    }

    #[test]
    fn nested_from_one_means_nested() {
        assert!(nested_from(Some("1")), "value '1' must be nested");
    }

    #[test]
    fn nested_from_arbitrary_nonempty_means_nested() {
        assert!(
            nested_from(Some("yes")),
            "non-empty non-zero value must be nested"
        );
        assert!(
            nested_from(Some("true")),
            "non-empty non-zero value must be nested"
        );
    }
}
