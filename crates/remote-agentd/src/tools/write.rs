//! remote_write tool — create or overwrite a file on the local filesystem.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Tool implementing file creation/overwrite.
pub struct WriteTool;

impl WriteTool {
    /// Execute the write tool.
    ///
    /// Input: `{ path: string, content: string }`
    /// Creates parent directories as needed and writes UTF-8 content.
    /// Returns a confirmation message naming the resolved path.
    pub fn execute(args: &Value) -> Result<Value> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`path` is required and must be a string"))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`content` is required and must be a string"))?;

        let path = Path::new(path_str);

        // Create parent directories if needed.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| anyhow!("Failed to create parent directories for {}: {}", path_str, e))?;
            }
        }

        fs::write(path, content.as_bytes())
            .map_err(|e| anyhow!("Failed to write {}: {}", path_str, e))?;

        let display = path.display();
        Ok(json!({
            "content": [{
                "type": "text",
                "text": format!("Wrote {} bytes to {}", content.len(), display)
            }]
        }))
    }
}
