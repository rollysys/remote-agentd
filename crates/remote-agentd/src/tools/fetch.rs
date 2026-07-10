//! remote_fetch tool — return metadata so the MCP client can pull a file
//! over a separate SSH/scp/rsync channel, bypassing the LLM context window.
//!
//! The daemon never streams file bytes over MCP. Instead it returns the
//! absolute path, size, checksum, and permission metadata. The client (pi,
//! claude-code, etc.) uses that path to open its own `scp`/`rsync` transfer,
//! keeping large files out of the JSON-RPC stream entirely.

use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::tools::sudo::{file_metadata, file_metadata_sudo, SudoMeta};

/// Tool implementing large-file fetch metadata.
pub struct FetchTool;

impl FetchTool {
    /// Execute the fetch tool.
    ///
    /// Input: `{ path: string, sudo?: bool }`
    /// Output: `{ path, abs_path, size, sha256, mode, owner, group }` — the
    /// client uses `abs_path` to `scp`/`rsync` the file over a side SSH channel.
    pub fn execute(args: &Value) -> Result<Value> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`path` is required and must be a string"))?;
        let sudo = args.get("sudo").and_then(|v| v.as_bool()).unwrap_or(false);

        let path = Path::new(path_str);

        // Resolve absolute path — this is what the client will feed to scp/rsync.
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| anyhow!("Cannot resolve cwd: {}", e))?
                .join(path)
        };

        // Stat the file (possibly via sudo) to confirm it exists and is readable.
        // We refuse directories — fetch is for single files only.
        let meta = if sudo {
            file_metadata_sudo(&abs_path)
        } else {
            file_metadata(&abs_path)
        }
        .map_err(|e| anyhow!("Cannot stat {}: {}", abs_path.display(), e))?;

        if meta.is_dir {
            return Err(anyhow!(
                "{} is a directory; remote_fetch only handles files. Use remote_read for directory listings.",
                abs_path.display()
            ));
        }

        // Compute sha256. For sudo-owned files we shell out to `sha256sum` since
        // we can't read the file directly from the daemon's uid.
        let sha256 = if sudo {
            compute_checksum_sudo(&abs_path)?
        } else {
            compute_checksum(&abs_path)?
        };

        let SudoMeta {
            mode,
            owner,
            group,
            ..
        } = meta;

        let text = format!(
            "remote_fetch: ready for sideband transfer\n\
             path:      {}\n\
             abs_path:  {}\n\
             size:      {} bytes\n\
             sha256:    {}\n\
             mode:      {:o}\n\
             owner:     {}\n\
             group:     {}\n\
             \n\
             Client: pull this file over a separate SSH channel, e.g.\n\
               scp user@host:{abs} ./local-name\n\
               rsync -e ssh user@host:{abs} ./local-name\n\
             Data does not transit the MCP/LLM channel.",
            path_str,
            abs_path.display(),
            meta.size,
            sha256,
            mode & 0o7777,
            owner,
            group,
            abs = abs_path.display()
        );

        // Also return structured metadata so programmatic clients can use it.
        Ok(json!({
            "content": [{
                "type": "text",
                "text": text
            }],
            "metadata": {
                "path": path_str,
                "abs_path": abs_path.to_string_lossy(),
                "size": meta.size,
                "sha256": sha256,
                "mode": format!("{:o}", mode & 0o7777),
                "owner": owner,
                "group": group
            }
        }))
    }
}

/// Compute sha256 via `sha256sum`/`shasum` (non-sudo path).
fn compute_checksum(path: &Path) -> Result<String> {
    compute_checksum_cmd(path, false)
}

/// Compute sha256 via `sudo sha256sum`/`sudo shasum` (sudo path).
fn compute_checksum_sudo(path: &Path) -> Result<String> {
    compute_checksum_cmd(path, true)
}

fn compute_checksum_cmd(path: &Path, sudo: bool) -> Result<String> {
    use std::process::Command;
    // Try sha256sum first (Linux), fall back to `shasum -a 256` (macOS).
    let mut tried = String::new();
    for (prog, args) in [("sha256sum", &[][..]), ("shasum", &["-a", "256"][..])] {
        let mut cmd = if sudo {
            let mut c = Command::new("sudo");
            c.arg("-n").arg(prog);
            c
        } else {
            Command::new(prog)
        };
        cmd.args(args).arg(path);
        match cmd.output() {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                // Both tools print "<hash>  <path>"
                if let Some(hash) = stdout.split_whitespace().next() {
                    return Ok(hash.to_string());
                }
            }
            Ok(out) => {
                tried.push_str(&format!(
                    "{}: {}\n",
                    prog,
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            Err(_) => {
                tried.push_str(&format!("{}: not found\n", prog));
            }
        }
    }
    Err(anyhow!(
        "Could not compute sha256 for {}: {}",
        path.display(),
        tried
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentd_fetch_test_{}_{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fetch_returns_metadata_for_file() {
        let d = tmp("meta");
        let f = d.join("data.bin");
        fs::write(&f, "hello world\n").unwrap();

        let args = json!({ "path": f.to_str().unwrap() });
        let res = FetchTool::execute(&args).unwrap();

        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("remote_fetch: ready for sideband transfer"));
        assert!(text.contains(&f.to_string_lossy().to_string()));
        assert!(text.contains("size:      12 bytes"));
        assert!(text.contains("sha256:"));

        let meta = &res["metadata"];
        assert_eq!(meta["size"], 12);
        assert_eq!(meta["abs_path"].as_str().unwrap(), &*f.to_string_lossy());
        assert!(meta["sha256"].as_str().unwrap().len() == 64);
    }

    #[test]
    fn fetch_rejects_directory() {
        let d = tmp("dir");
        let args = json!({ "path": d.to_str().unwrap() });
        let res = FetchTool::execute(&args);
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("directory"));
    }

    #[test]
    fn fetch_sha256_matches_external() {
        let d = tmp("sha");
        let f = d.join("known.txt");
        let content = "The quick brown fox\n";
        fs::write(&f, content).unwrap();

        let args = json!({ "path": f.to_str().unwrap() });
        let res = FetchTool::execute(&args).unwrap();
        let hash = res["metadata"]["sha256"].as_str().unwrap();

        // Verify against an independent computation using the same tool.
        let recomputed = compute_checksum_cmd(&f, false).unwrap();
        assert_eq!(hash, recomputed);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn fetch_missing_path_errors() {
        let args = json!({ "path": "/nonexistent/path/no/such/file" });
        let res = FetchTool::execute(&args);
        assert!(res.is_err());
    }

    #[test]
    fn fetch_includes_client_instructions() {
        let d = tmp("instr");
        let f = d.join("x.txt");
        fs::write(&f, "x").unwrap();

        let args = json!({ "path": f.to_str().unwrap() });
        let res = FetchTool::execute(&args).unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("scp") || text.contains("rsync"));
        assert!(text.contains("does not transit the MCP"));
    }
}
