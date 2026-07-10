//! remote_write tool — create or overwrite a file, with sudo + permission support.

use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::tools::sudo::{mkdir_all, set_owner_mode, write_file};

/// Tool implementing file creation/overwrite.
pub struct WriteTool;

impl WriteTool {
    /// Execute the write tool.
    ///
    /// Input: `{ path, content, sudo?: bool, mode?: string, owner?: string }`
    /// Creates parent directories as needed and writes UTF-8 content.
    /// When `sudo` is true, writes via `sudo -n tee` instead of direct fs::write.
    /// When `mode`/`owner` are given, applies them after writing (chmod/chown).
    pub fn execute(args: &Value) -> Result<Value> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`path` is required and must be a string"))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`content` is required and must be a string"))?;
        let sudo = args.get("sudo").and_then(|v| v.as_bool()).unwrap_or(false);
        let mode = args.get("mode").and_then(|v| v.as_str());
        let owner = args.get("owner").and_then(|v| v.as_str());

        let path = Path::new(path_str);

        // Create parent directories if needed (sudo-aware).
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                mkdir_all(parent, sudo)
                    .map_err(|e| anyhow!("Failed to create parent directories for {}: {}", path_str, e))?;
            }
        }

        // Write file content (sudo-aware: uses `sudo -n tee` when sudo=true).
        write_file(path, content, sudo)
            .map_err(|e| anyhow!("Failed to write {}: {}", path_str, e))?;

        // Apply mode/owner if requested.
        if mode.is_some() || owner.is_some() {
            // Default mode if only owner is given: preserve current (0 means "don't chmod").
            // But set_owner_mode always chmods; if mode is None, use the file's current mode.
            let resolved_mode: u32 = if let Some(m) = mode {
                u32::from_str_radix(m.trim_start_matches("0o"), 8)
                    .map_err(|e| anyhow!("Invalid mode '{}': {}", m, e))?
            } else {
                // No mode given: read current and re-apply (effectively a no-op chmod).
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::metadata(path)
                        .map(|m| m.permissions().mode())
                        .unwrap_or(0o644)
                }
                #[cfg(not(unix))]
                { 0o644 }
            };
            set_owner_mode(path, resolved_mode, owner, sudo)
                .map_err(|e| anyhow!("Failed to set mode/owner on {}: {}", path_str, e))?;
        }

        let display = path.display();
        let mut msg = format!("Wrote {} bytes to {}", content.len(), display);
        if sudo {
            msg.push_str(" (via sudo)");
        }
        if let Some(m) = mode {
            msg.push_str(&format!(", mode={}", m));
        }
        if let Some(o) = owner {
            msg.push_str(&format!(", owner={}", o));
        }
        Ok(json!({
            "content": [{
                "type": "text",
                "text": msg
            }]
        }))
    }
}
