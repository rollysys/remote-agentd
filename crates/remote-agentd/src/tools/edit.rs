//! remote_edit tool — apply hashline patch edits.

use std::fs;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use hashline::parser::parse_sections;
use hashline::{Patch, Patcher};

/// Tool implementing hashline patch application.
pub struct EditTool;

impl EditTool {
    /// Execute the edit tool.
    ///
    /// Input: `{ input: string }` — a hashline patch.
    /// Parses sections, applies via `hashline::Patcher`, returns a result text
    /// per section: the new `[path#TAG]` header plus a diff preview.
    pub fn execute(args: &Value) -> Result<Value> {
        let input = args
            .get("input")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`input` is required and must be a string"))?;

        let sections = parse_sections(input);
        if sections.is_empty() {
            return Err(anyhow!(
                "No hashline sections found in input. A patch must contain at least one `[path#TAG]` section."
            ));
        }

        let patch = Patch { sections };

        // Apply the patch. The Patcher reads each section's file off disk,
        // validates the snapshot tag, applies edits in memory, then writes.
        let mut patcher = Patcher::new();
        patcher
            .apply(&patch)
            .map_err(|e| anyhow!("Patch application failed: {}", e))?;

        // Build a result summary: one block per section with the new tag and a
        // short diff preview of the changed region.
        let mut out = String::new();
        for section in &patch.sections {
            // Read the (possibly updated) file to compute the fresh tag.
            match fs::read_to_string(&section.path) {
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
