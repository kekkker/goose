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
use std::sync::Arc;
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

impl ServerHandler for SummonMcpServer {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "goose-summon",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Goose summon extension: use `delegate` to run subagent tasks and \
                 `load` to list/read available recipes and agents."
                    .to_string(),
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
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

    let server = SummonMcpServer::new().await?;
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("summon mcp server error: {:?}", e))?;
    service.waiting().await?;
    Ok(())
}
