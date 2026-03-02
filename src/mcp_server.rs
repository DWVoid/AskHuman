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
use tokio_util::sync::CancellationToken;
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
// Server lifecycle command (sent from the UI to the background runtime)
// ---------------------------------------------------------------------------

/// Commands the UI sends to the background server-management loop.
#[derive(Debug)]
pub enum ServerCommand {
    /// Start (or restart) the MCP server on the given port.
    Start(u16),
    /// Stop the currently-running server, if any.
    Stop,
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
        ctx: RequestContext<RoleServer>,
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

        // If the client included a progress token, spawn a keepalive task that
        // sends periodic progress notifications so long-running waits don't
        // time out on the agent side.
        let keepalive = ctx.meta.get_progress_token().map(|token| {
            let peer = ctx.peer.clone();
            let ct = ctx.ct.clone();
            tokio::spawn(async move {
                let mut step: f64 = 0.0;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                            step += 1.0;
                            let _ = peer.notify_progress(ProgressNotificationParam {
                                progress_token: token.clone(),
                                progress: step,
                                total: None,
                                message: Some("Waiting for human operator…".to_string()),
                            }).await;
                        }
                        _ = ct.cancelled() => break,
                    }
                }
            })
        });

        // Wait for the user's answer, honouring client-side cancellation.
        let result = tokio::select! {
            r = answer_rx => match r {
                Ok(Some(answer)) => Ok(CallToolResult::success(vec![Content::text(answer)])),
                Ok(None) => Ok(CallToolResult::error(vec![Content::text(
                    "The human operator declined to answer.",
                )])),
                Err(_) => Err(McpError::internal_error(
                    "Answer channel closed unexpectedly",
                    None,
                )),
            },
            _ = ctx.ct.cancelled() => {
                Err(McpError::internal_error("Request cancelled by client", None))
            }
        };

        if let Some(handle) = keepalive {
            handle.abort();
        }
        result
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
// Background server management loop
// ---------------------------------------------------------------------------

/// Runs in the background tokio runtime.  Listens for [`ServerCommand`]s from
/// the UI and starts/stops the Streamable HTTP server accordingly.
pub async fn server_loop(
    question_tx: mpsc::Sender<QuestionRequest>,
    mut cmd_rx: mpsc::Receiver<ServerCommand>,
) {
    let mut current_ct: Option<CancellationToken> = None;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            ServerCommand::Start(port) => {
                if let Some(ct) = current_ct.take() {
                    ct.cancel();
                }
                let ct = CancellationToken::new();
                current_ct = Some(ct.clone());
                let tx = question_tx.clone();
                tokio::spawn(async move {
                    run_server(tx, port, ct).await;
                });
            }
            ServerCommand::Stop => {
                if let Some(ct) = current_ct.take() {
                    ct.cancel();
                }
            }
        }
    }
}

/// Runs a single Streamable HTTP MCP server instance until `ct` is cancelled.
///
/// Binds to `127.0.0.1:<port>`, sets up the rmcp [`StreamableHttpService`],
/// and exits gracefully when the provided [`CancellationToken`] is cancelled
/// (e.g. via [`ServerCommand::Stop`] or a restart triggered by
/// [`ServerCommand::Start`]).
async fn run_server(question_tx: mpsc::Sender<QuestionRequest>, port: u16, ct: CancellationToken) {
    let service = StreamableHttpService::new(
        move || {
            let tx = question_tx.clone();
            Ok(AskHumanServer::new(tx))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig {
            cancellation_token: ct.child_token(),
            ..Default::default()
        },
    );

    let addr = format!("127.0.0.1:{port}");
    let router = axum::Router::new().nest_service("/mcp", service);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("AskHuman: failed to bind {addr}: {e}");
            return;
        }
    };

    eprintln!("AskHuman MCP server  →  http://{addr}/mcp");

    let _ = axum::serve(listener, router)
        .with_graceful_shutdown(async move { ct.cancelled().await })
        .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::{ClientHandler, ServiceExt, model::RawContent};
    use tokio::sync::mpsc;

    /// Helper: create a fresh AskHumanServer with an in-process question channel.
    fn make_server() -> (AskHumanServer, mpsc::Receiver<QuestionRequest>) {
        let (tx, rx) = mpsc::channel(8);
        (AskHumanServer::new(tx), rx)
    }

    // ----- Unit: channel routing (duplex / stdio transport) -----

    /// The `ask_human` tool should forward the question to the UI channel and
    /// return the answer provided via the oneshot sender.
    #[tokio::test]
    async fn test_ask_human_answer() {
        let (server, mut q_rx) = make_server();

        let (server_transport, client_transport) = tokio::io::duplex(4096);

        let server_clone = server.clone();
        let server_task = tokio::spawn(async move {
            let svc = server_clone.serve(server_transport).await.expect("serve");
            svc.waiting().await
        });

        // `()` implements ClientHandler as a no-op.
        let client = ().serve(client_transport).await.expect("client serve");

        // Call the tool in a background task (it blocks until answered).
        let peer = client.peer().clone();
        let call_task = tokio::spawn(async move {
            peer.call_tool(CallToolRequestParams {
                meta: None,
                name: "ask_human".into(),
                arguments: Some({
                    let mut m = serde_json::Map::new();
                    m.insert(
                        "question".into(),
                        serde_json::Value::String("What is your name?".into()),
                    );
                    m
                }),
                task: None,
            })
            .await
        });

        // Auto-answer from the "UI side".
        let req = q_rx.recv().await.expect("question received");
        assert_eq!(req.question, "What is your name?");
        let tx = req.answer_tx.lock().unwrap().take().unwrap();
        tx.send(Some("Alice".into())).unwrap();

        let result = call_task.await.unwrap().expect("tool call ok");
        assert!(!result.content.is_empty());
        if let RawContent::Text(t) = &result.content[0].raw {
            assert_eq!(t.text, "Alice");
        } else {
            panic!("unexpected content type: {:?}", result.content);
        }

        client.cancel().await.unwrap();
        let _ = server_task.await;
    }

    /// A rejected question should return a tool error result (not a JSON-RPC error).
    #[tokio::test]
    async fn test_ask_human_reject() {
        let (server, mut q_rx) = make_server();
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let _server_handle = tokio::spawn(async move {
            let svc = server.serve(server_transport).await.expect("serve");
            let _ = svc.waiting().await;
        });
        let client = ().serve(client_transport).await.expect("client serve");
        let peer = client.peer().clone();
        let call_task = tokio::spawn(async move {
            peer.call_tool(CallToolRequestParams {
                meta: None,
                name: "ask_human".into(),
                arguments: Some({
                    let mut m = serde_json::Map::new();
                    m.insert(
                        "question".into(),
                        serde_json::Value::String("Yes/no?".into()),
                    );
                    m
                }),
                task: None,
            })
            .await
        });
        let req = q_rx.recv().await.unwrap();
        req.answer_tx.lock().unwrap().take().unwrap().send(None).unwrap();
        let result = call_task.await.unwrap().expect("tool call ok");
        assert_eq!(result.is_error, Some(true));
        client.cancel().await.unwrap();
    }

    // ----- Integration: full Streamable HTTP stack -----

    /// Verify that `server_loop` starts an HTTP server and responds to an
    /// `initialize` request.  Uses raw `reqwest` to avoid pulling in the rmcp
    /// HTTP client feature.
    #[tokio::test]
    async fn test_server_loop_start_stop() {
        let (q_tx, _q_rx) = mpsc::channel::<QuestionRequest>(8);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ServerCommand>(4);

        tokio::spawn(server_loop(q_tx, cmd_rx));

        // Pick a free port.
        let tmp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tmp.local_addr().unwrap().port();
        drop(tmp);

        cmd_tx.send(ServerCommand::Start(port)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
            )
            .send()
            .await
            .expect("HTTP request");

        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(body.contains("\"jsonrpc\":\"2.0\""), "body: {body}");

        cmd_tx.send(ServerCommand::Stop).await.unwrap();
    }
}
