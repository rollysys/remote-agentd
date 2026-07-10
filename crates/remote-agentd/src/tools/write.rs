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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentd_write_test_{}_{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_basic_creates_file() {
        let d = tmp("basic");
        let f = d.join("basic.txt");

        let args = json!({ "path": f.to_str().unwrap(), "content": "hello\n" });
        let res = WriteTool::execute(&args).unwrap();

        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Wrote 6 bytes"));
        assert_eq!(fs::read_to_string(&f).unwrap(), "hello\n");
    }

    #[test]
    fn write_creates_parent_dirs() {
        let d = tmp("parents");
        let f = d.join("a/b/c/deep.txt");

        let args = json!({ "path": f.to_str().unwrap(), "content": "deep\n" });
        WriteTool::execute(&args).unwrap();

        assert!(f.exists());
        assert_eq!(fs::read_to_string(&f).unwrap(), "deep\n");
    }

    #[test]
    fn write_overwrites_existing() {
        let d = tmp("overwrite");
        let f = d.join("over.txt");
        fs::write(&f, "old content").unwrap();

        let args = json!({ "path": f.to_str().unwrap(), "content": "new" });
        WriteTool::execute(&args).unwrap();

        assert_eq!(fs::read_to_string(&f).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn write_with_mode_param() {
        use std::os::unix::fs::PermissionsExt;

        let d = tmp("mode");
        let f = d.join("moded.txt");

        let args = json!({
            "path": f.to_str().unwrap(),
            "content": "data",
            "mode": "600"
        });
        WriteTool::execute(&args).unwrap();

        let mode = fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode should be 0600");
    }

    #[cfg(unix)]
    #[test]
    fn write_with_mode_0o_prefix() {
        use std::os::unix::fs::PermissionsExt;

        let d = tmp("modeo");
        let f = d.join("moded2.txt");

        let args = json!({
            "path": f.to_str().unwrap(),
            "content": "data",
            "mode": "0o755"
        });
        WriteTool::execute(&args).unwrap();

        let mode = fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "mode should be 0755 with 0o prefix");
    }

    #[test]
    fn write_mode_param_in_result_text() {
        let d = tmp("mode_text");
        let f = d.join("mt.txt");

        let args = json!({
            "path": f.to_str().unwrap(),
            "content": "x",
            "mode": "644"
        });
        let res = WriteTool::execute(&args).unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("mode=644"));
    }

    #[test]
    fn write_invalid_mode_errors() {
        let d = tmp("bad_mode");
        let f = d.join("bad.txt");

        let args = json!({
            "path": f.to_str().unwrap(),
            "content": "x",
            "mode": "not-a-number"
        });
        let res = WriteTool::execute(&args);
        assert!(res.is_err());
    }

    #[test]
    fn write_missing_content_errors() {
        let d = tmp("no_content");
        let f = d.join("nc.txt");

        let args = json!({ "path": f.to_str().unwrap() });
        let res = WriteTool::execute(&args);
        assert!(res.is_err());
    }
}
