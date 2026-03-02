//! MCP Streamable HTTP transport (2025-03-26 spec)
//!
//! Single endpoint POST /mcp handles all JSON-RPC messages.
//! Long-running tool calls (ask_human) respond with SSE when the client
//! includes `Accept: text/event-stream`.

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, RwLock};
use std::collections::HashSet;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types shared with the UI
// ---------------------------------------------------------------------------

/// A question sent from the MCP server to the desktop UI.
pub struct QuestionRequest {
    /// Unique ID for this question card.
    pub id: String,
    /// The question text to display.
    pub question: String,
    /// Optional context shown below the question.
    pub context: Option<String>,
    /// Oneshot sender – UI sends `Some(answer)` or `None` (reject).
    pub answer_tx: Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>,
}

// Manual impls so Message: Debug + Clone (required by iced)
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
// Server state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ServerState {
    /// Active session IDs (assigned at initialize).
    sessions: Arc<RwLock<HashSet<String>>>,
    /// Channel used to push new questions to the UI.
    question_tx: mpsc::Sender<QuestionRequest>,
}

// ---------------------------------------------------------------------------
// JSON-RPC request type
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(question_tx: mpsc::Sender<QuestionRequest>, port: u16) {
    let state = ServerState {
        sessions: Arc::new(RwLock::new(HashSet::new())),
        question_tx,
    };

    let app = Router::new()
        .route("/mcp", post(mcp_post))
        .route("/mcp", get(mcp_get))
        .route("/mcp", delete(mcp_delete))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", port);
    println!("AskHuman MCP server  →  http://{}/mcp", addr);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to bind {}: {}", addr, e);
            std::process::exit(1);
        });

    axum::serve(listener, app).await.unwrap();
}

// ---------------------------------------------------------------------------
// POST /mcp  – all client messages
// ---------------------------------------------------------------------------

async fn mcp_post(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let request: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                json_response(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") }
                })),
            )
                .into_response();
        }
    };

    // Notifications are fire-and-forget
    if request.method.starts_with("notifications/") {
        return StatusCode::ACCEPTED.into_response();
    }

    let id = request.id.clone().unwrap_or(Value::Null);
    let accepts_sse = accepts_sse(&headers);

    match request.method.as_str() {
        "initialize" => {
            let session_id = Uuid::new_v4().to_string();
            state.sessions.write().await.insert(session_id.clone());

            let body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "ask-human",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            });
            let mut resp = json_response(body).into_response();
            resp.headers_mut().insert(
                "mcp-session-id",
                session_id.parse().unwrap(),
            );
            resp
        }

        "tools/list" => json_response(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [{
                    "name": "ask_human",
                    "description":
                        "Display a question in the AskHuman desktop window and \
                         wait for the human operator to type a reply.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The question to ask the human"
                            },
                            "context": {
                                "type": "string",
                                "description": "Optional extra context shown below the question"
                            }
                        },
                        "required": ["question"]
                    }
                }]
            }
        }))
        .into_response(),

        "tools/call" => handle_tools_call(state, id, request.params, accepts_sse).await,

        _ => json_response(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {}", request.method)
            }
        }))
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// tools/call dispatcher
// ---------------------------------------------------------------------------

async fn handle_tools_call(
    state: ServerState,
    id: Value,
    params: Option<Value>,
    accepts_sse: bool,
) -> Response {
    let params = match params {
        Some(p) => p,
        None => {
            return json_response(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32600, "message": "Missing params" }
            }))
            .into_response();
        }
    };

    let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if tool_name != "ask_human" {
        return json_response(json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("Unknown tool: {tool_name}") }
        }))
        .into_response();
    }

    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("(no question provided)")
        .to_string();
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .map(str::to_string);

    // Create oneshot to receive the answer from the UI
    let (answer_tx, answer_rx) = oneshot::channel::<Option<String>>();
    let req = QuestionRequest {
        id: Uuid::new_v4().to_string(),
        question,
        context,
        answer_tx: Arc::new(Mutex::new(Some(answer_tx))),
    };

    if state.question_tx.send(req).await.is_err() {
        return json_response(json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32603, "message": "UI unavailable" }
        }))
        .into_response();
    }

    // Block until the user answers (or rejects / window closes)
    let rpc_response = match answer_rx.await {
        Ok(Some(answer)) => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "content": [{ "type": "text", "text": answer }],
                "isError": false
            }
        }),
        Ok(None) => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "content": [{ "type": "text",
                    "text": "The human operator declined to answer." }],
                "isError": true
            }
        }),
        Err(_) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32603, "message": "Answer channel closed unexpectedly" }
        }),
    };

    if accepts_sse {
        // Wrap the response in a single SSE `message` event, then close.
        let data = serde_json::to_string(&rpc_response).unwrap_or_default();
        let body = format!("event: message\ndata: {data}\n\n");
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(body))
            .unwrap()
    } else {
        json_response(rpc_response).into_response()
    }
}

// ---------------------------------------------------------------------------
// GET /mcp  – optional server-initiated SSE stream
// ---------------------------------------------------------------------------

async fn mcp_get(headers: HeaderMap) -> Response {
    if !accepts_sse(&headers) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    // Return an open SSE stream kept alive by comments.  We have no
    // server-initiated messages to send, so we just keep it alive.
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// DELETE /mcp  – session termination
// ---------------------------------------------------------------------------

async fn mcp_delete(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> StatusCode {
    if let Some(id) = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        state.sessions.write().await.remove(id);
    }
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn accepts_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false)
}

fn json_response(body: Value) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}
