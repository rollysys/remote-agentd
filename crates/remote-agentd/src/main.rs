//! Remote Agent Daemon — MCP stdio server for remote file operations.
//!
//! Reads newline-delimited JSON-RPC from stdin, dispatches via `McpHandler`,
//! and writes responses to stdout (flushed after each message batch).
//!
//! Uses synchronous `std::io` — no async runtime required. Stdio is
//! inherently sequential, so an async event loop adds no value here.
//!
//! Logging goes to stderr so it never corrupts the JSON-RPC stdout stream.

mod mcp;
mod tools;

use std::io::{self, BufRead, Write};

use mcp::McpHandler;

fn main() -> anyhow::Result<()> {
    // Wire up tool handlers. Each tool's static `execute` is wrapped in a
    // trait impl in `tools/mod.rs` and registered here.
    let mut handler = McpHandler::new();
    tools::register_all(&mut handler);

    let stdin = io::stdin();
    let stdout = io::stdout();
    let stdin_lock = stdin.lock();
    let mut stdout_lock = stdout.lock();

    for line in stdin_lock.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("remote-agentd: stdin read error: {e}");
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        // `handle_message` returns a vec of outgoing JSON-RPC messages
        // (progress notifications + final response, in order).
        let messages = handler.handle_message(&line);
        if messages.is_empty() {
            continue;
        }

        for msg in &messages {
            let json = serde_json::to_string(msg)?;
            writeln!(stdout_lock, "{json}")?;
        }
        stdout_lock.flush()?;
    }

    Ok(())
}
