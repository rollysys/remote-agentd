//! remote_put tool — two-phase commit for uploading large files to the remote
//! host via a sideband SSH/scp/rsync channel, bypassing the LLM context window.
//!
//! Phase 1 (default): daemon creates a temp staging path and returns it.
//!   The client `scp`/`rsync` uploads the file there over a separate SSH channel.
//! Phase 2 (`commit: true`): daemon atomically renames the staged file into its
//!   final destination, applies mode/owner, and cleans up.
//!
//! This keeps file bytes off the MCP stream while still letting the daemon
//! control permissions atomically.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::tools::sudo::{
    file_metadata, file_metadata_sudo, mkdir_all, rename, set_owner_mode, touch, SudoMeta,
};

/// Staging directory for in-progress uploads. Lives under /tmp so it's
/// on the same filesystem as most destinations (rename is atomic only
/// within a single filesystem).
const STAGING_DIR: &str = "/tmp/remote-agentd-staging";

/// Tool implementing large-file put (upload) via sideband + commit.
pub struct PutTool;

impl PutTool {
    pub fn execute(args: &Value) -> Result<Value> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`path` is required and must be a string"))?;
        let sudo = args.get("sudo").and_then(|v| v.as_bool()).unwrap_or(false);
        let commit = args.get("commit").and_then(|v| v.as_bool()).unwrap_or(false);

        let dest = Path::new(path_str);

        if !commit {
            // Phase 1: prepare staging path.
            Self::prepare_stage(dest, sudo)
        } else {
            // Phase 2: commit — rename staged file into place, apply metadata.
            let staging_path = args
                .get("staging_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!(
                    "`staging_path` is required for commit phase (returned by phase 1)"
                ))?;
            let mode = args.get("mode").and_then(|v| v.as_str());
            let owner = args.get("owner").and_then(|v| v.as_str());
            Self::commit(dest, Path::new(staging_path), sudo, mode, owner)
        }
    }

    /// Phase 1: create staging dir + return a unique staging path for the
    /// client to upload to.
    fn prepare_stage(dest: &Path, sudo: bool) -> Result<Value> {
        mkdir_all(std::path::Path::new(STAGING_DIR), sudo)
            .map_err(|e| anyhow!("Failed to create staging dir {}: {}", STAGING_DIR, e))?;

        // Unique staging name based on destination + pid + timestamp.
        let staging_name = format!(
            "{}.{}.{}",
            dest.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "upload".to_string()),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let staging_path = PathBuf::from(STAGING_DIR).join(&staging_name);

        // Create an empty file so the staging path exists (some clients expect
        // this); the client will overwrite it via scp/rsync.
        touch(&staging_path, sudo)
            .map_err(|e| anyhow!("Failed to create staging file {}: {}", staging_path.display(), e))?;

        let abs_dest = if dest.is_absolute() {
            dest.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| anyhow!("Cannot resolve cwd: {}", e))?
                .join(dest)
        };

        let text = format!(
            "remote_put: staging ready (phase 1/2)\n\
             dest:         {}\n\
             staging_path: {}\n\
             \n\
             Client: upload the file to the staging path over a separate SSH channel:\n\
               scp ./local-file user@host:{stage}\n\
               rsync -e ssh ./local-file user@host:{stage}\n\
             Then call remote_put again with {{path, commit: true}} to finalize.\n\
             Data does not transit the MCP/LLM channel.",
            abs_dest.display(),
            staging_path.display(),
            stage = staging_path.display()
        );

        Ok(json!({
            "content": [{
                "type": "text",
                "text": text
            }],
            "metadata": {
                "dest": abs_dest.to_string_lossy(),
                "staging_path": staging_path.to_string_lossy(),
                "phase": 1
            }
        }))
    }

    /// Phase 2: rename staged file → dest, create parent dirs, apply mode/owner.
    ///
    /// The staged file must already exist (client uploaded it via scp/rsync).
    /// We create parent dirs of dest if needed, then `rename(2)` the staged
    /// file into place (atomic on the same filesystem — /tmp staging ensures
    /// this for most dests), then apply mode/owner.
    fn commit(
        dest: &Path,
        staging: &Path,
        sudo: bool,
        mode: Option<&str>,
        owner: Option<&str>,
    ) -> Result<Value> {
        let abs_dest = if dest.is_absolute() {
            dest.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| anyhow!("Cannot resolve cwd: {}", e))?
                .join(dest)
        };

        // Verify staged file exists and has content.
        let stage_meta = if sudo {
            file_metadata_sudo(staging)
        } else {
            file_metadata(staging)
        }
        .map_err(|e| anyhow!("Staged file {} not found: {}", staging.display(), e))?;
        if stage_meta.is_dir {
            return Err(anyhow!("Staging path {} is a directory, expected a file", staging.display()));
        }
        if stage_meta.size == 0 {
            return Err(anyhow!(
                "Staged file {} is empty — did the client upload to it? \
                 (phase 1 creates an empty placeholder; the client must scp/rsync \
                 the real content over it before calling commit)",
                staging.display()
            ));
        }
        if let Some(parent) = abs_dest.parent() {
            if !parent.as_os_str().is_empty() {
                mkdir_all(parent, sudo)
                    .map_err(|e| anyhow!("Failed to create parent dirs for {}: {}", abs_dest.display(), e))?;
            }
        }

        // Capture original metadata if dest already exists (for edit-style
        // preservation when mode/owner are not explicitly given).

        let orig_meta: Option<SudoMeta> = if sudo {
            file_metadata_sudo(&abs_dest).ok()
        } else {
            file_metadata(&abs_dest).ok()
        };
        rename(staging, &abs_dest, sudo)
            .map_err(|e| anyhow!("Failed to rename {} → {}: {}", staging.display(), abs_dest.display(), e))?;

        // Resolve mode: explicit > original > default 0644.
        let resolved_mode: u32 = if let Some(m) = mode {
            u32::from_str_radix(m.trim_start_matches("0o"), 8)
                .map_err(|e| anyhow!("Invalid mode '{}': {}", m, e))?
        } else if let Some(om) = orig_meta.as_ref().map(|m| m.mode) {
            om
        } else {
            0o644
        };

        // Resolve owner: explicit > original > leave as-is.
        let resolved_owner = owner
            .map(String::from)
            .or_else(|| orig_meta.as_ref().map(|m| m.owner.clone()));

        // Apply mode + owner.
        set_owner_mode(&abs_dest, resolved_mode, resolved_owner.as_deref(), sudo)
            .map_err(|e| anyhow!("Failed to set metadata on {}: {}", abs_dest.display(), e))?;

        let final_meta = if sudo {
            file_metadata_sudo(&abs_dest)
        } else {
            file_metadata(&abs_dest)
        }
        .map_err(|e| anyhow!("Post-commit stat failed for {}: {}", abs_dest.display(), e))?;

        let text = format!(
            "remote_put: committed (phase 2/2)\n\
             dest:    {}\n\
             size:    {} bytes\n\
             mode:    {:o}\n\
             owner:   {}\n\
             group:   {}\n\
             staged:  {} (removed)",
            abs_dest.display(),
            final_meta.size,
            final_meta.mode & 0o7777,
            final_meta.owner,
            final_meta.group,
            staging.display()
        );

        Ok(json!({
            "content": [{
                "type": "text",
                "text": text
            }],
            "metadata": {
                "dest": abs_dest.to_string_lossy(),
                "size": final_meta.size,
                "mode": format!("{:o}", final_meta.mode & 0o7777),
                "owner": final_meta.owner,
                "group": final_meta.group,
                "phase": 2
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentd_put_test_{}_{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn phase1_creates_staging_path() {
        let d = tmp("phase1");
        let dest = d.join("output.txt");

        let args = json!({ "path": dest.to_str().unwrap() });
        let res = PutTool::execute(&args).unwrap();

        let meta = &res["metadata"];
        assert_eq!(meta["phase"], 1);
        assert_eq!(meta["dest"].as_str().unwrap(), &*dest.to_string_lossy());

        let staging = meta["staging_path"].as_str().unwrap();
        assert!(staging.starts_with("/tmp/remote-agentd-staging/"));
        assert!(Path::new(staging).exists(), "staging file should exist after phase 1");
        assert_eq!(fs::metadata(staging).unwrap().len(), 0, "staging file should be empty placeholder");
    }

    #[test]
    fn phase1_text_includes_scp_instructions() {
        let d = tmp("instr");
        let dest = d.join("out.bin");

        let args = json!({ "path": dest.to_str().unwrap() });
        let res = PutTool::execute(&args).unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("staging_path"));
        assert!(text.contains("scp") || text.contains("rsync"));
        assert!(text.contains("phase 1/2"));
    }

    #[test]
    fn commit_without_staging_path_errors() {
        let d = tmp("no_staging");
        let dest = d.join("out.txt");

        let args = json!({ "path": dest.to_str().unwrap(), "commit": true });
        let res = PutTool::execute(&args);
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("staging_path"));
    }

    #[test]
    fn commit_renames_and_applies_default_mode() {
        let d = tmp("commit");
        let dest = d.join("final.txt");

        // Phase 1: create staging.
        let args1 = json!({ "path": dest.to_str().unwrap() });
        let res1 = PutTool::execute(&args1).unwrap();
        let staging = res1["metadata"]["staging_path"].as_str().unwrap();

        // Simulate client upload: write content to staging path.
        fs::write(staging, "uploaded content\n").unwrap();

        // Phase 2: commit.
        let args2 = json!({
            "path": dest.to_str().unwrap(),
            "commit": true,
            "staging_path": staging
        });
        let res2 = PutTool::execute(&args2).unwrap();

        let meta = &res2["metadata"];
        assert_eq!(meta["phase"], 2);
        assert_eq!(meta["size"], 17);
        assert_eq!(meta["dest"].as_str().unwrap(), &*dest.to_string_lossy());

        // File should now exist at destination with correct content.
        assert!(dest.exists(), "dest file should exist after commit");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "uploaded content\n");
        assert!(!Path::new(staging).exists(), "staging file should be removed after commit");
    }

    #[test]
    fn commit_with_explicit_mode() {
        let d = tmp("mode");
        let dest = d.join("mode_test.txt");

        // Phase 1.
        let staging = PutTool::execute(&json!({ "path": dest.to_str().unwrap() }))
            .unwrap()["metadata"]["staging_path"]
            .as_str()
            .unwrap()
            .to_string();

        fs::write(&staging, "data").unwrap();

        // Phase 2 with mode 0o600.
        let args2 = json!({
            "path": dest.to_str().unwrap(),
            "commit": true,
            "staging_path": staging,
            "mode": "600"
        });
        let res2 = PutTool::execute(&args2).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode should be 0600 after commit");
        }

        let meta = &res2["metadata"];
        assert_eq!(meta["mode"], "600");
    }

    #[test]
    fn commit_empty_staging_errors() {
        let d = tmp("empty");
        let dest = d.join("empty.txt");

        // Phase 1 creates empty staging.
        let staging = PutTool::execute(&json!({ "path": dest.to_str().unwrap() }))
            .unwrap()["metadata"]["staging_path"]
            .as_str()
            .unwrap()
            .to_string();

        // Don't upload anything — staging is still empty (0 bytes).
        let args2 = json!({
            "path": dest.to_str().unwrap(),
            "commit": true,
            "staging_path": staging
        });
        let res = PutTool::execute(&args2);
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("empty"));
    }

    #[test]
    fn commit_creates_parent_dirs() {
        let d = tmp("parents");
        let dest = d.join("nested/deep/dir/output.txt");

        let staging = PutTool::execute(&json!({ "path": dest.to_str().unwrap() }))
            .unwrap()["metadata"]["staging_path"]
            .as_str()
            .unwrap()
            .to_string();

        fs::write(&staging, "content").unwrap();

        let args2 = json!({
            "path": dest.to_str().unwrap(),
            "commit": true,
            "staging_path": staging
        });
        PutTool::execute(&args2).unwrap();

        assert!(dest.exists());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "content");
    }
}
