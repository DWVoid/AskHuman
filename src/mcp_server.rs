//! MCP server using the official `rmcp` crate with Streamable HTTP transport.
//!
//! Exposes a single `/mcp` endpoint handled by [`rmcp`]'s
//! [`StreamableHttpService`], which fully implements the MCP 2025-03-26 /
//! 2025-11-25 Streamable HTTP specification.

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    },
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types shared with the UI layer
// ---------------------------------------------------------------------------

/// A question forwarded from the MCP server to the desktop UI.
pub struct QuestionRequest {
    /// Unique ID used to identify the card in the UI.
    pub id: String,
    /// Question text shown on the card.
    pub question: String,
    /// Optional context shown below the question.
    pub context: Option<String>,
    /// Oneshot sender – the UI sends `Some(answer)` or `None` (rejected).
    pub answer_tx: Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>,
}

// Manual Debug + Clone impls (iced Message requires both, and oneshot::Sender
// implements neither).
impl std::fmt::Debug for QuestionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuestionRequest")
            .field("id", &self.id)
            .field("question", &self.question)
            .finish()
    }
}
impl Clone for QuestionRequest {
    fn clone(&self) -> Self {
        QuestionRequest {
            id: self.id.clone(),
            question: self.question.clone(),
            context: self.context.clone(),
            answer_tx: Arc::clone(&self.answer_tx),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool input schema
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AskHumanParams {
    /// The question to display to the human operator.
    question: String,
    /// Optional extra context shown below the question.
    context: Option<String>,
}

// ---------------------------------------------------------------------------
// MCP server handler
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AskHumanServer {
    question_tx: mpsc::Sender<QuestionRequest>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl AskHumanServer {
    fn new(question_tx: mpsc::Sender<QuestionRequest>) -> Self {
        Self {
            question_tx,
            tool_router: Self::tool_router(),
        }
    }

    /// Display a question in the AskHuman desktop window and wait for the
    /// human operator to type a reply.
    #[tool(description = "Display a question in the AskHuman desktop window and wait for the human operator to type a reply.")]
    async fn ask_human(
        &self,
        Parameters(AskHumanParams { question, context }): Parameters<AskHumanParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let (answer_tx, answer_rx) = oneshot::channel::<Option<String>>();
        let req = QuestionRequest {
            id: Uuid::new_v4().to_string(),
            question,
            context,
            answer_tx: Arc::new(Mutex::new(Some(answer_tx))),
        };

        self.question_tx
            .send(req)
            .await
            .map_err(|_| McpError::internal_error("UI unavailable", None))?;

        match answer_rx.await {
            Ok(Some(answer)) => Ok(CallToolResult::success(vec![Content::text(answer)])),
            Ok(None) => Ok(CallToolResult::error(vec![Content::text(
                "The human operator declined to answer.",
            )])),
            Err(_) => Err(McpError::internal_error(
                "Answer channel closed unexpectedly",
                None,
            )),
        }
    }
}

#[tool_handler]
impl ServerHandler for AskHumanServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                ..Default::default()
            },
            instructions: Some(
                "Ask the human operator a question and wait for their reply.".to_string(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point called from main
// ---------------------------------------------------------------------------

pub async fn run(question_tx: mpsc::Sender<QuestionRequest>, port: u16) {
    let service = StreamableHttpService::new(
        move || {
            let tx = question_tx.clone();
            Ok(AskHumanServer::new(tx))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );

    let addr = format!("127.0.0.1:{}", port);
    println!("AskHuman MCP server  →  http://{}/mcp", addr);

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to bind {}: {}", addr, e);
            std::process::exit(1);
        });

    axum::serve(listener, router).await.unwrap();
}
