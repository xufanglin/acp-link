//! MCP Server (Streamable HTTP)
//!
//! 以 HTTP 服务形式运行，对外暴露 `/mcp` endpoint，
//! 实现 MCP Streamable HTTP transport 规范。

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::im::IMChannel;

/// MCP Server 共享状态
struct McpState {
    channel: Arc<dyn IMChannel>,
    /// 活跃 session ID（简单实现：仅支持单 session）
    session_id: RwLock<Option<String>>,
}

/// 启动 MCP HTTP Server（作为后台 task 运行）
pub async fn start_mcp_server(channel: Arc<dyn IMChannel>, port: u16) -> Result<()> {
    let state = Arc::new(McpState {
        channel,
        session_id: RwLock::new(None),
    });

    let app = Router::new()
        .route("/mcp", post(handle_post))
        .route("/mcp", get(handle_get))
        .route("/mcp", delete(handle_delete))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("MCP HTTP Server 监听: http://127.0.0.1:{port}/mcp");

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server 退出: {e}"))
}

/// POST /mcp — 接收 JSON-RPC 请求
async fn handle_post(
    State(state): State<Arc<McpState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let req: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                None,
                make_error(&Value::Null, -32700, &format!("Parse error: {e}")),
            );
        }
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    // notification（无 id）→ 202 Accepted
    if req.get("id").is_none() {
        return (StatusCode::ACCEPTED, "").into_response();
    }

    // 非 initialize 请求需要验证 session
    if method != "initialize" {
        let expected = state.session_id.read().await;
        if let Some(ref sid) = *expected {
            let client_sid = headers.get("mcp-session-id").and_then(|v| v.to_str().ok());
            if client_sid != Some(sid.as_str()) {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    None,
                    make_error(&id, -32600, "Missing or invalid Mcp-Session-Id"),
                );
            }
        }
    }

    let resp = match method {
        "initialize" => {
            let sid = uuid::Uuid::new_v4().to_string();
            *state.session_id.write().await = Some(sid);
            handle_initialize(&id, &state)
        }
        "tools/list" => handle_tools_list(&id, &state),
        "tools/call" => handle_tools_call(&id, &params, &state).await,
        _ => make_error(&id, -32601, &format!("Method not found: {method}")),
    };

    let current_sid = state.session_id.read().await.clone();
    json_response(StatusCode::OK, current_sid.as_deref(), resp)
}

/// GET /mcp — SSE stream（当前不需要 server-initiated 消息，返回 405）
async fn handle_get() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, "SSE not supported").into_response()
}

/// DELETE /mcp — 终止 session
async fn handle_delete(State(state): State<Arc<McpState>>, headers: HeaderMap) -> Response {
    let expected = state.session_id.read().await.clone();
    let client_sid = headers.get("mcp-session-id").and_then(|v| v.to_str().ok());

    if expected.as_deref() == client_sid {
        *state.session_id.write().await = None;
        (StatusCode::OK, "").into_response()
    } else {
        (StatusCode::NOT_FOUND, "").into_response()
    }
}

// ── JSON-RPC handlers ──────────────────────────────────────────

fn handle_initialize(id: &Value, state: &McpState) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": format!("{}-mcp", state.channel.platform_name()),
                "version": "0.1.0"
            }
        }
    })
}

fn handle_tools_list(id: &Value, state: &McpState) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": state.channel.mcp_tool_list()
        }
    })
}

async fn handle_tools_call(id: &Value, params: &Value, state: &McpState) -> Value {
    let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match state.channel.mcp_tool_call(tool_name, &args).await {
        Ok(data) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": data.to_string() }]
            }
        }),
        Err(msg) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": msg }],
                "isError": true
            }
        }),
    }
}

// ── helpers ─────────────────────────────────────────────────────

fn json_response(status: StatusCode, session_id: Option<&str>, body: Value) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    if let Some(sid) = session_id {
        builder = builder.header("mcp-session-id", sid);
    }
    builder
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "").into_response())
}

fn make_error(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── make_error ──────────────────────────────────────────────────────────

    #[test]
    fn test_make_error_structure() {
        let err = make_error(&json!(1), -32600, "Invalid Request");
        assert_eq!(err["jsonrpc"], "2.0");
        assert_eq!(err["id"], 1);
        assert_eq!(err["error"]["code"], -32600);
        assert_eq!(err["error"]["message"], "Invalid Request");
    }

    #[test]
    fn test_make_error_null_id() {
        let err = make_error(&Value::Null, -32700, "Parse error");
        assert!(err["id"].is_null());
    }

    // ── test helper ────────────────────────────────────────────────────────

    fn test_state() -> McpState {
        use crate::im::FeishuChannel;
        let channel = FeishuChannel::new("test_id", "test_secret");
        McpState {
            channel: Arc::new(channel),
            session_id: RwLock::new(None),
        }
    }

    // ── handle_initialize ───────────────────────────────────────────────────

    #[test]
    fn test_handle_initialize_response() {
        let state = test_state();
        let resp = handle_initialize(&json!(1), &state);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        let result = &resp["result"];
        assert_eq!(result["serverInfo"]["name"], "feishu-mcp");
        assert!(result["capabilities"]["tools"].is_object());
    }

    // ── handle_tools_list ───────────────────────────────────────────────────

    #[test]
    fn test_handle_tools_list_response() {
        let state = test_state();
        let resp = handle_tools_list(&json!(2), &state);
        assert_eq!(resp["id"], 2);
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        assert_eq!(tools[0]["name"], "feishu_send_file");
    }

    // ── json_response ───────────────────────────────────────────────────────

    #[test]
    fn test_json_response_with_session_id() {
        let resp = json_response(StatusCode::OK, Some("sid-123"), json!({"ok": true}));
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("mcp-session-id")
                .unwrap()
                .to_str()
                .unwrap(),
            "sid-123"
        );
    }

    #[test]
    fn test_json_response_without_session_id() {
        let resp = json_response(StatusCode::BAD_REQUEST, None, json!({"error": "bad"}));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(resp.headers().get("mcp-session-id").is_none());
    }

    // ── handle_get / handle_delete ──────────────────────────────────────────

    #[tokio::test]
    async fn test_handle_get_returns_method_not_allowed() {
        let resp = handle_get().await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
