//! Tool registry and dispatch.
//!
//! Defines the `Tool` trait that each tool implementation follows,
//! the `ToolRegistry` for dispatch, and the static tool definitions
//! (schemas) for `tools/list`. Wire-compatible with the Python prototype
//! at `remote_agentd.py`.

pub mod read;
pub mod edit;
pub mod search;
pub mod bash;
pub mod find;
pub mod write;

use serde_json::{json, Value};

// ═══════════════════════════════════════════════════════════════════════════════
// Tool trait
// ═══════════════════════════════════════════════════════════════════════════════

/// A tool that can be invoked via `tools/call`.
///
/// Each tool module (`read.rs`, `edit.rs`, …) provides a struct that
/// implements this trait. The `ToolRegistry` dispatches by name.
#[allow(dead_code)]
pub trait Tool: Send {
    /// Tool name (e.g. `"remote_read"`).
    fn name(&self) -> &'static str;

    /// Human-readable description shown in `tools/list`.
    fn description(&self) -> &'static str;

    /// JSON Schema for the tool's `arguments` object.
    fn input_schema(&self) -> Value;

    /// Execute the tool. Returns a `ToolResult` that the MCP layer
    /// serializes into the `tools/call` response.
    fn execute(&mut self, args: &Value, ctx: &mut ToolContext) -> ToolResult;
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tool execution context
// ═══════════════════════════════════════════════════════════════════════════════
/// Context passed to `Tool::execute`.
///
/// Carries the snapshot store (needed by read/edit/write for hashline
/// tag verification) and an optional progress sender (needed by
/// bash/search for streaming output).
#[allow(dead_code)]
pub struct ToolContext<'a> {
    /// Mutable access to the shared snapshot store.
    pub snapshots: &'a mut hashline::InMemorySnapshotStore,

    /// Progress sender — present when the client provided a
    /// `progressToken` in the `_meta` of the `tools/call` params.
    pub progress: Option<ProgressSender<'a>>,
}

#[allow(dead_code)]
impl<'a> ToolContext<'a> {
    /// Get the progress sender if a progress token was provided.
    pub fn progress(&mut self) -> Option<&mut ProgressSender<'a>> {
        self.progress.as_mut()
    }
}

/// Sends `notifications/progress` messages during tool execution.
///
/// Each call to `send` pushes a JSON-RPC notification onto the outgoing
/// buffer. The MCP layer drains this buffer *before* writing the final
/// `tools/call` response, preserving ordering.
#[allow(dead_code)]
pub struct ProgressSender<'a> {
    token: Value,
    out: &'a mut Vec<Value>,
}
#[allow(dead_code)]
impl<'a> ProgressSender<'a> {
    pub fn new(token: Value, out: &'a mut Vec<Value>) -> Self {
        Self { token, out }
    }

    /// Emit a progress notification.
    ///
    /// `total` of `0` means "unknown total" (matching the Python prototype,
    /// which passes `0` when the total is not known).
    pub fn send(&mut self, progress: u64, total: u64, message: &str) {
        self.out.push(json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": self.token,
                "progress": progress,
                "total": total,
                "message": message
            }
        }));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tool result
// ═══════════════════════════════════════════════════════════════════════════════

/// Result of a tool execution — maps to the MCP `tools/call` result object.
pub struct ToolResult {
    /// Content blocks (text). Matches `{"type":"text","text":"…"}`.
    pub content: Vec<ContentBlock>,

    /// `true` when the tool ran but the operation itself failed
    /// (e.g. non-zero exit code from `remote_bash`). Maps to `"isError"`.
    pub is_error: bool,
}

impl ToolResult {
    /// Successful result with a single text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: false,
        }
    }

    /// Error result with a single text block.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: true,
        }
    }

    /// Convert to the MCP `result` JSON value for `tools/call`.
    pub fn to_json(&self) -> Value {
        json!({
            "content": self.content.iter().map(ContentBlock::to_json).collect::<Vec<_>>(),
            "isError": self.is_error
        })
    }
}

/// A single content block in a tool result.
pub struct ContentBlock {
    /// The text content.
    pub text: String,
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    pub fn to_json(&self) -> Value {
        json!({ "type": "text", "text": self.text })
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tool registry
// ═══════════════════════════════════════════════════════════════════════════════

/// A registered tool: static definition (always present) + optional handler.
struct ToolEntry {
    name: &'static str,
    description: &'static str,
    schema: Value,
    handler: Option<Box<dyn Tool>>,
}

/// Maps tool names to definitions and (optionally) handler implementations.
///
/// `tools/list` always returns all definitions, even before handlers are
/// registered. `tools/call` dispatches to the handler if present.
pub struct ToolRegistry {
    entries: Vec<ToolEntry>,
}

impl ToolRegistry {
    /// Create a registry pre-populated with all 6 tool definitions.
    /// Handlers are `None` until `register` is called for each tool.
    pub fn new() -> Self {
        Self {
            entries: vec![
                ToolEntry {
                    name: "remote_read",
                    description: "Read files on the remote host with line selectors and hashline tags. \
                         Supports directories (tree listing), line ranges (:50-100), raw mode (:raw). \
                         Output format matches pi's read tool: [path#TAG] header + numbered lines.",
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Remote file/directory path, may include :selector"
                            }
                        },
                        "required": ["path"]
                    }),
                    handler: None,
                },
                ToolEntry {
                    name: "remote_edit",
                    description: "Apply hashline patch edits to remote files. \
                         Input is the hashline patch language: [PATH#TAG] sections with SWAP/DEL/INS ops. \
                         TAG must match the latest remote_read output.",
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Hashline patch language input"
                            }
                        },
                        "required": ["input"]
                    }),
                    handler: None,
                },
                ToolEntry {
                    name: "remote_search",
                    description: "Search file contents with regex on the remote host. \
                         Uses ripgrep when available, falls back to Rust regex. \
                         Output format: [path#TAG] headers with *LINE:content match lines.",
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "pattern": {
                                "type": "string",
                                "description": "Regex pattern"
                            },
                            "paths": {
                                "type": ["string", "array"],
                                "description": "Files/dirs/globs to search"
                            },
                            "case": {
                                "type": "boolean",
                                "description": "Case-sensitive (default true)"
                            },
                            "gitignore": {
                                "type": "boolean",
                                "description": "Respect .gitignore (default true)"
                            }
                        },
                        "required": ["pattern"]
                    }),
                    handler: None,
                },
                ToolEntry {
                    name: "remote_bash",
                    description: "Execute a shell command on the remote host with streaming output. \
                         Each output line is streamed via progress notification. \
                         Final result includes exit code.",
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "command": {
                                "type": "string",
                                "description": "Shell command to execute"
                            },
                            "cwd": {
                                "type": "string",
                                "description": "Working directory"
                            },
                            "timeout": {
                                "type": "number",
                                "description": "Timeout in seconds (default 60)"
                            },
                            "env": {
                                "type": "object",
                                "description": "Environment variables"
                            }
                        },
                        "required": ["command"]
                    }),
                    handler: None,
                },
                ToolEntry {
                    name: "remote_find",
                    description: "Find files matching glob patterns on the remote host.",
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "paths": {
                                "type": "array",
                                "description": "Glob patterns or directories"
                            },
                            "gitignore": {
                                "type": "boolean",
                                "description": "Respect .gitignore (default true)"
                            },
                            "hidden": {
                                "type": "boolean",
                                "description": "Include hidden files (default true)"
                            }
                        },
                        "required": ["paths"]
                    }),
                    handler: None,
                },
                ToolEntry {
                    name: "remote_write",
                    description: "Create or overwrite a file on the remote host.",
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Remote file path"
                            },
                            "content": {
                                "type": "string",
                                "description": "File content"
                            }
                        },
                        "required": ["path", "content"]
                    }),
                    handler: None,
                },
            ],
        }
    }

    /// Register a handler for a tool. The tool name must match an existing
    /// definition; otherwise this is a no-op.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name();
        if let Some(entry) = self.entries.iter_mut().find(|e| e.name == name) {
            entry.handler = Some(tool);
        }
    }

    /// List all tool definitions as JSON (for `tools/list`).
    pub fn list(&self) -> Vec<Value> {
        self.entries
            .iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "description": e.description,
                    "inputSchema": e.schema
                })
            })
            .collect()
    }

    /// Check whether a tool name is known (exists in definitions).
    pub fn has_tool(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// Get a mutable reference to a tool's handler, if registered.
    pub fn get_handler_mut(&mut self, name: &str) -> Option<&mut Box<dyn Tool>> {
        self.entries
            .iter_mut()
            .find(|e| e.name == name)
            .and_then(|e| e.handler.as_mut())
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tool wiring — trait impls that delegate to each tool module's static
// `execute(args) -> anyhow::Result<Value>`. The concrete tool logic lives
// in read.rs/edit.rs/search.rs/bash.rs/find.rs/write.rs (owned by another
// agent); these wrappers bridge them into the `Tool` trait so the MCP
// dispatcher can call them polymorphically.
//
// ToolImpl's static methods return a JSON `Value` already shaped as the MCP
// `tools/call` result object (`{"content":[{"type":"text","text":"…"}],
// "isError": <bool>}`). We parse that back into `ToolResult` so the handler
// stays in control of serialization.
// ═══════════════════════════════════════════════════════════════════════════════

/// Convert a tool-module JSON result into a `ToolResult`.
///
/// Accepts either:
///   - `{"content":[{"type":"text","text":"…"}], "isError": bool}` (full), or
///   - a bare string (treated as a single text block), or
///   - any other JSON (stringified as a single text block).
fn result_from_json(v: Value) -> ToolResult {
    // Full MCP result object.
    if let Some(obj) = v.as_object() {
        if obj.contains_key("content") {
            let content = obj
                .get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|block| {
                            block
                                .get("text")
                                .and_then(|t| t.as_str())
                                .map(ContentBlock::text)
 })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let is_error = obj
                .get("isError")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            return ToolResult { content, is_error };
        }
    }
    // Bare string → single text block.
    if let Some(s) = v.as_str() {
        return ToolResult::text(s);
    }
    // Fallback: stringify.
    ToolResult::text(v.to_string())
}

/// Build a `Tool` trait impl wrapper around a tool module's static
/// `<Struct>::execute(args) -> anyhow::Result<Value>` method.
macro_rules! tool_wrapper {
    ($wrapper:ident, $tool_mod:ident, $impl_struct:ident, $tool_name:literal) => {
        struct $wrapper;

        impl Tool for $wrapper {
            fn name(&self) -> &'static str {
                $tool_name
            }

            fn description(&self) -> &'static str {
                // Description lives in the registry; the trait method is only
                // for ad-hoc introspection. Return the name as a fallback.
                $tool_name
            }

            fn input_schema(&self) -> Value {
                // Schema is owned by the registry; the MCP layer always reads
                // schemas from `ToolRegistry::list()`.
                json!({})
            }

            fn execute(&mut self, args: &Value, _ctx: &mut ToolContext) -> ToolResult {
                match $tool_mod::$impl_struct::execute(args) {
                    Ok(v) => result_from_json(v),
                    Err(e) => ToolResult::error(format!("{e:#}")),
                }
            }
        }
    };
}

tool_wrapper!(ReadToolWrapper, read, ReadTool, "remote_read");
tool_wrapper!(EditToolWrapper, edit, EditTool, "remote_edit");
tool_wrapper!(SearchToolWrapper, search, SearchTool, "remote_search");
tool_wrapper!(BashToolWrapper, bash, BashTool, "remote_bash");
tool_wrapper!(FindToolWrapper, find, FindTool, "remote_find");
tool_wrapper!(WriteToolWrapper, write, WriteTool, "remote_write");

/// Register all 6 tool handlers with the MCP handler.
/// Called from `main.rs` at startup.
pub fn register_all(handler: &mut crate::mcp::McpHandler) {
    handler.register_tool(Box::new(ReadToolWrapper));
    handler.register_tool(Box::new(EditToolWrapper));
    handler.register_tool(Box::new(SearchToolWrapper));
    handler.register_tool(Box::new(BashToolWrapper));
    handler.register_tool(Box::new(FindToolWrapper));
    handler.register_tool(Box::new(WriteToolWrapper));
}
