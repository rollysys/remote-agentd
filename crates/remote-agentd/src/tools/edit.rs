//! remote_edit tool — apply hashline patch edits, with optional sudo support.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use hashline::parser::parse_sections;
use hashline::{Patch, PatchSection, Patcher};

use crate::tools::sudo::{self, SudoMeta};

/// Tool implementing hashline patch application.
pub struct EditTool;

impl EditTool {
    /// Execute the edit tool.
    ///
    /// Input: `{ input: string, sudo?: bool }` — a hashline patch.
    /// Parses sections, applies via `hashline::Patcher`, returns a result text
    /// per section: the new `[path#TAG]` header plus a diff preview.
    ///
    /// When `sudo` is true, each section's file is read via `sudo -n cat` into
    /// a temp file, the patch is applied to the temp file, then the result is
    /// written back via `sudo -n tee` with original mode/owner restored.
    pub fn execute(args: &Value) -> Result<Value> {
        let input = args
            .get("input")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`input` is required and must be a string"))?;
        let sudo_flag = args.get("sudo").and_then(|v| v.as_bool()).unwrap_or(false);

        let mut sections = parse_sections(input);
        if sections.is_empty() {
            return Err(anyhow!(
                "No hashline sections found in input. A patch must contain at least one `[path#TAG]` section."
            ));
        }

        // sudo path: remap each section's path to a temp file, apply, then
        // write back and restore metadata.
        if sudo_flag {
            return Self::execute_sudo(&mut sections);
        }

        let patch = Patch { sections };

        // Apply the patch. The Patcher reads each section's file off disk,
        // validates the snapshot tag, applies edits in memory, then writes.
        let mut patcher = Patcher::new();
        patcher
            .apply(&patch)
            .map_err(|e| anyhow!("Patch application failed: {}", e))?;

        Self::build_result(&patch.sections)
    }

    /// sudo variant: for each section, copy the root-owned file to a temp
    /// file (daemon-readable), apply the patch to temp files, then write
    /// results back to the original paths via sudo with metadata restored.
    fn execute_sudo(sections: &mut Vec<PatchSection>) -> Result<Value> {
        // Map: original path → temp path. We rewrite section.path to temp,
        // apply the patch, then copy each temp file back to the original.
        let mut temp_map: HashMap<String, PathBuf> = HashMap::new();
        let mut meta_map: HashMap<String, SudoMeta> = HashMap::new();

        for section in sections.iter_mut() {
            // Skip REM/MV-only sections that don't read a file (FileOp::Rem
            // deletes; we handle that specially).
            let orig_path = section.path.clone();

            // Capture original metadata for later restoration.
            if let Ok(meta) = sudo::file_metadata_sudo(std::path::Path::new(&orig_path)) {
                meta_map.insert(orig_path.clone(), meta);
            }

            // Read via sudo into a temp file.
            let content = sudo::read_file(std::path::Path::new(&orig_path), true)
                .map_err(|e| anyhow!("sudo read {} failed: {}", orig_path, e))?;

            let temp_path = PathBuf::from(format!(
                "/tmp/remote-agentd-edit-{}-{}",
                std::process::id(),
                temp_map.len()
            ));
            std::fs::write(&temp_path, content.as_bytes())
                .map_err(|e| anyhow!("Failed to write temp file {}: {}", temp_path.display(), e))?;

            temp_map.insert(orig_path, temp_path.clone());

            // Rewrite the section's path to point to the temp file.
            // The hashline tag is content-based, so it still matches.
            section.path = temp_path.to_string_lossy().into_owned();
        }

        let patch = Patch { sections: sections.clone() };
        let mut patcher = Patcher::new();
        patcher
            .apply(&patch)
            .map_err(|e| anyhow!("Patch application (sudo) failed: {}", e))?;

        // Write each temp file back to its original path via sudo, restoring
        // mode/owner.
        for (orig_path, temp_path) in &temp_map {
            let content = std::fs::read_to_string(temp_path)
                .map_err(|e| anyhow!("Failed to read temp {} for writeback: {}", temp_path.display(), e))?;
            sudo::write_file(std::path::Path::new(orig_path), &content, true)
                .map_err(|e| anyhow!("sudo writeback to {} failed: {}", orig_path, e))?;

            // Restore original mode/owner.
            if let Some(meta) = meta_map.get(orig_path) {
                let owner = format!("{}:{}", meta.owner, meta.group);
                let _ = sudo::set_owner_mode(
                    std::path::Path::new(orig_path),
                    meta.mode,
                    Some(&owner),
                    true,
                );
            }

            // Clean up temp file.
            let _ = std::fs::remove_file(temp_path);
        }

        // Build result using the ORIGINAL paths (not temp paths).
        let mut result_sections: Vec<PatchSection> = patch.sections;
        for section in result_sections.iter_mut() {
            // Reverse-map temp path → original path.
            for (orig, temp) in &temp_map {
                if section.path == temp.to_string_lossy() {
                    section.path = orig.clone();
                    break;
                }
            }
        }

        Self::build_result(&result_sections)
    }
    /// Build the result text: one block per section with the new tag and
    /// a short diff preview of the changed region.
    fn build_result(sections: &[PatchSection]) -> Result<Value> {
        let mut out = String::new();
        for section in sections {
            // Read the (possibly updated) file to compute the fresh tag.
            match std::fs::read_to_string(&section.path) {
                Ok(content) => {
                    let tag = hashline::compute_file_hash(&content);
                    let header = hashline::format_hashline_header(&section.path, &tag);
                    out.push_str(&header);
                    out.push('\n');

                    // Diff preview: show first ~40 lines of the new content.
                    let preview_lines: Vec<&str> = content.lines().take(40).collect();
                    for (i, line) in preview_lines.iter().enumerate() {
                        out.push_str(&format!("{}:{}\n", i + 1, line));
                    }
                    let total = content.lines().count();
                    if total > 40 {
                        out.push_str(&format!("…\n[{} lines total — showing first 40]\n", total));
                    }
                }
                Err(_) => {
                    // File may have been deleted (REM) or moved (MV).
                    out.push_str(&format!("[{}]\n", section.path));
                    if let Some(op) = &section.file_op {
                        match op {
                            hashline::FileOp::Rem => {
                                out.push_str("(file removed)\n");
                            }
                            hashline::FileOp::Mv { dest } => {
                                out.push_str(&format!("(moved to {})\n", dest));
                            }
                        }
                    }
                }
            }

            if !section.warnings.is_empty() {
                out.push_str("warnings:\n");
                for w in &section.warnings {
                    out.push_str(&format!("  - {}\n", w));
                }
            }
            out.push('\n');
        }

        Ok(json!({
            "content": [{
                "type": "text",
                "text": out
            }]
        }))
    }
}
