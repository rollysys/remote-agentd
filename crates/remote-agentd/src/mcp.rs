//! MCP (Model Context Protocol) stdio transport.
//!
//! Wire-compatible with the Python prototype at `remote_agentd.py`.
//! Implements JSON-RPC 2.0 over newline-delimited stdin/stdout.
//!
//! Supported methods:
//!   - `initialize`              → server info + capabilities
//!   - `notifications/initialized` → no response (notification)
//!   - `tools/list`              → 8 tool definitions
//!   - `ping`                    → empty result
//!
//! Unknown methods with an `id` → JSON-RPC error -32601.

use serde_json::{json, Value};

use crate::tools::{ToolContext, ToolRegistry, ToolResult};

// ═══════════════════════════════════════════════════════════════════════════════
// JSON-RPC error codes
// ═══════════════════════════════════════════════════════════════════════════════

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const _INTERNAL_ERROR: i64 = -32603; // reserved for tool-internal failures

// ═══════════════════════════════════════════════════════════════════════════════
// McpHandler
// ═══════════════════════════════════════════════════════════════════════════════

/// MCP protocol handler. Owns the snapshot store and tool registry.
///
/// `handle_message` parses one JSON-RPC line and returns zero or more
/// outgoing JSON values:
///   - `None`     → nothing to send (notification or parse error already
///                  reported via the returned buffer — but our model returns
///                  `Vec<Value>` so the caller writes them all).
///   - `Some(vec)` → one or more messages to write to stdout, in order.
///
/// Progress notifications (if any) precede the final response.
pub struct McpHandler {
    /// Shared snapshot store for read/edit/write tag verification.
    snapshots: hashline::InMemorySnapshotStore,

    /// Tool registry — definitions + handlers.
    tools: ToolRegistry,
}

impl McpHandler {
    pub fn new() -> Self {
        Self {
            snapshots: hashline::InMemorySnapshotStore::new(),
            tools: ToolRegistry::new(),
        }
    }

    /// Register a tool handler. Delegates to the registry.
    pub fn register_tool(&mut self, tool: Box<dyn crate::tools::Tool>) {
        self.tools.register(tool);
    }

    /// Handle one incoming JSON-RPC line.
    ///
    /// Returns a vector of outgoing JSON-RPC messages (responses and/or
    /// notifications), in the order they should be written. An empty vec
    /// means "nothing to send" (e.g. for notifications).
    pub fn handle_message(&mut self, line: &str) -> Vec<Value> {
        // Parse the JSON line.
        let parsed: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return vec![json_error(Value::Null, PARSE_ERROR, &format!("Parse error: {e}"))];
            }
        };

        // Must be an object.
        let obj = match parsed.as_object() {
            Some(o) => o,
            None => {
                return vec![json_error(
                    Value::Null,
                    INVALID_REQUEST,
                    "Invalid Request: not a JSON object",
                )];
            }
        };

        // Validate jsonrpc version (lenient: accept missing or "2.0").
        if let Some(v) = obj.get("jsonrpc") {
            if v.as_str() != Some("2.0") {
                let id = obj.get("id").cloned().unwrap_or(Value::Null);
                return vec![json_error(id, INVALID_REQUEST, "Invalid jsonrpc version")];
            }
        }

        let method = obj.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = obj.get("id").cloned(); // None for notifications
        let params = obj.get("params").cloned().unwrap_or(Value::Null);

        match method {
            // ── initialize ──────────────────────────────────────────────
            "initialize" => {
                let id = id.unwrap_or(Value::Null);
                vec![json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {
                            "tools": { "listChanged": false }
                        },
                        "serverInfo": {
                            "name": "remote-agentd",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }
                })]
            }

            // ── notifications/initialized ───────────────────────────────
            "notifications/initialized" => {
                // Notification: no response.
                vec![]
            }

            // ── tools/list ──────────────────────────────────────────────
            "tools/list" => {
                let id = id.unwrap_or(Value::Null);
                vec![json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": self.tools.list()
                    }
                })]
            }

            // ── tools/call ──────────────────────────────────────────────
            "tools/call" => self.handle_tools_call(id, params),

            // ── ping ────────────────────────────────────────────────────
            "ping" => {
                let id = id.unwrap_or(Value::Null);
                vec![json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {}
                })]
            }

            // ── unknown method ──────────────────────────────────────────
            _ => {
                if let Some(id) = id {
                    vec![json_error(
                        id,
                        METHOD_NOT_FOUND,
                        &format!("Unknown method: {method}"),
                    )]
                } else {
                    // Unknown notification — silently ignore (no id, no response).
                    vec![]
                }
            }
        }
    }

    /// Dispatch a `tools/call` request.
    fn handle_tools_call(&mut self, id: Option<Value>, params: Value) -> Vec<Value> {
        let id = id.unwrap_or(Value::Null);

        let params_obj = match params.as_object() {
            Some(o) => o,
            None => {
                return vec![json_error(id, INVALID_PARAMS, "Invalid params")];
            }
        };

        let tool_name = params_obj
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("");

        if tool_name.is_empty() {
            return vec![json_error(id, INVALID_PARAMS, "Missing tool name")];
        }

        let args = params_obj
            .get("arguments")
            .cloned()
            .unwrap_or(json!({}));

        // Extract progress token from _meta.progressToken (if present).
        let progress_token = params_obj
            .get("_meta")
            .and_then(|m| m.get("progressToken"))
            .cloned();

        // Check that the tool exists in the registry definitions.
        if !self.tools.has_tool(tool_name) {
            return vec![json_error(
                id,
                METHOD_NOT_FOUND,
                &format!("Unknown tool: {tool_name}"),
            )];
        }

        // Buffer for progress notifications emitted during execution.
        let mut progress_out: Vec<Value> = Vec::new();

        // Build the execution context.
        let result: ToolResult = {
            // Borrow snapshots mutably for the context.
            let mut ctx = ToolContext {
                snapshots: &mut self.snapshots,
                progress: progress_token.map(|tok| {
                    crate::tools::ProgressSender::new(tok, &mut progress_out)
                }),
            };

            // Dispatch to the registered handler, if any.
            match self.tools.get_handler_mut(tool_name) {
                Some(handler) => handler.execute(&args, &mut ctx),
                None => ToolResult::error(format!(
                    "Tool '{tool_name}' is defined but has no implementation registered"
                )),
            }
        };

        // Build the final response. Progress notifications come first.
        let mut out = progress_out;
        out.push(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result.to_json()
        }));
        out
    }
}

impl Default for McpHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Build a JSON-RPC error response.
fn json_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_id(handler: &mut McpHandler, method: &str, params: Value) -> Value {
        let line = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        })
        .to_string();
        let out = handler.handle_message(&line);
        assert_eq!(out.len(), 1, "expected exactly one response for {method}");
        out.into_iter().next().unwrap()
    }

    #[test]
    fn initialize_returns_server_info() {
        let mut h = McpHandler::new();
        let resp = req_id(&mut h, "initialize", json!({}));
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], "remote-agentd");
        assert_eq!(resp["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], false);
    }

    #[test]
    fn tools_list_returns_eight_tools() {
        let mut h = McpHandler::new();
        let resp = req_id(&mut h, "tools/list", json!({}));
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 8);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"remote_read"));
        assert!(names.contains(&"remote_edit"));
        assert!(names.contains(&"remote_search"));
        assert!(names.contains(&"remote_bash"));
        assert!(names.contains(&"remote_find"));
        assert!(names.contains(&"remote_write"));
        assert!(names.contains(&"remote_fetch"));
        assert!(names.contains(&"remote_put"));
    }

    #[test]
    fn ping_returns_empty_result() {
        let mut h = McpHandler::new();
        let resp = req_id(&mut h, "ping", json!({}));
        assert_eq!(resp["id"], 1);
        assert!(resp["result"].as_object().unwrap().is_empty());
    }

    #[test]
    fn initialized_notification_no_response() {
        let mut h = McpHandler::new();
        let line = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        })
        .to_string();
        let out = h.handle_message(&line);
        assert!(out.is_empty(), "notification should produce no response");
    }

    #[test]
    fn unknown_method_returns_error() {
        let mut h = McpHandler::new();
        let resp = req_id(&mut h, "nonexistent/method", json!({}));
        assert_eq!(resp["error"]["code"], -32601);
        assert_eq!(
            resp["error"]["message"],
            "Unknown method: nonexistent/method"
        );
    }

    #[test]
    fn parse_error_returns_error() {
        let mut h = McpHandler::new();
        let out = h.handle_message("this is not json {{{");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["error"]["code"], -32700);
    }

    #[test]
    fn unknown_tool_returns_error() {
        let mut h = McpHandler::new();
        let resp = req_id(
            &mut h,
            "tools/call",
            json!({ "name": "no_such_tool", "arguments": {} }),
        );
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn unregistered_tool_returns_iserror() {
        // remote_read is defined but no handler is registered → isError result.
        let mut h = McpHandler::new();
        let resp = req_id(
            &mut h,
            "tools/call",
            json!({ "name": "remote_read", "arguments": { "path": "." } }),
        );
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no implementation"));
    }

    #[test]
    fn missing_tool_name_returns_error() {
        let mut h = McpHandler::new();
        let resp = req_id(&mut h, "tools/call", json!({ "arguments": {} }));
        assert_eq!(resp["error"]["code"], -32602);
    }
}
