//! remote_search tool — regex search across files using the `ignore` crate.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{json, Value};

use hashline::compute_file_hash;

/// Match limit for the whole search.
const MAX_TOTAL_MATCHES: usize = 2000;
/// Per-file match limit when searching multiple files.
const MAX_PER_FILE_MULTI: usize = 20;
/// Per-file match limit when searching a single file.
const MAX_PER_FILE_SINGLE: usize = 200;
/// Maximum characters emitted per matched/context line.
const MAX_LINE_COLUMNS: usize = 512;

/// Tool implementing regex search across files.
pub struct SearchTool;

impl SearchTool {
    /// Execute the search tool.
    ///
    /// Input: `{ pattern: string, paths?: string|array, case?: bool, gitignore?: bool }`
    /// Output: per-file blocks `[path#TAG]\n*LINE:match\n LINE:context`.
    pub fn execute(args: &Value) -> Result<Value> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`pattern` is required and must be a string"))?;

        let case_sensitive = args.get("case").and_then(|v| v.as_bool()).unwrap_or(true);
        let gitignore = args.get("gitignore").and_then(|v| v.as_bool()).unwrap_or(true);

        // Collect paths: accept string or array, default to ["."].
        let paths: Vec<String> = match args.get("paths") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => vec![".".to_string()],
            _ => return Err(anyhow!("`paths` must be a string or array")),
        };

        // Build the regex.
        let mut re_builder = Regex::new(pattern)
            .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;
        if !case_sensitive {
            // Rebuild case-insensitive by wrapping with (?i).
            let ci = format!("(?i){}", pattern);
            re_builder = Regex::new(&ci)
                .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;
        }
        let re = re_builder;

        let single_file = paths.len() == 1 && Path::new(&paths[0]).is_file();
        let per_file_limit = if single_file {
            MAX_PER_FILE_SINGLE
        } else {
            MAX_PER_FILE_MULTI
        };

        // Walk each path and gather candidate files.
        let mut files: Vec<PathBuf> = Vec::new();
        for root in &paths {
            let root_path = Path::new(root);
            if root_path.is_file() {
                files.push(root_path.to_path_buf());
                continue;
            }
            let walker = WalkBuilder::new(root_path)
                .hidden(true)
                .ignore(gitignore)
                .git_ignore(gitignore)
                .git_global(gitignore)
                .git_exclude(gitignore)
                .build();
            for entry in walker {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if entry.file_type().map_or(false, |ft| ft.is_file()) {
                    files.push(entry.into_path());
                }
            }
        }

        let mut output = String::new();
        let mut total_matches = 0usize;

        for file_path in &files {
            if total_matches >= MAX_TOTAL_MATCHES {
                break;
            }
            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let lines: Vec<&str> = content.lines().collect();

            let mut file_matches = 0usize;
            let mut file_block = String::new();
            let tag = compute_file_hash(&content);
            let header = hashline::format_hashline_header(
                &file_path.display().to_string(),
                &tag,
            );
            file_block.push_str(&header);
            file_block.push('\n');

            for (idx, line) in lines.iter().enumerate() {
                if total_matches >= MAX_TOTAL_MATCHES || file_matches >= per_file_limit {
                    break;
                }
                if re.is_match(line) {
                    file_matches += 1;
                    total_matches += 1;
                    let line_no = idx + 1;
                    file_block.push_str(&format!(
                        "*{}:{}\n",
                        line_no,
                        truncate(line, MAX_LINE_COLUMNS)
                    ));
                    // 1 leading + 3 trailing context lines.
                    if idx > 0 {
                        file_block.push_str(&format!(
                            " {}:{}\n",
                            line_no - 1,
                            truncate(lines[idx - 1], MAX_LINE_COLUMNS)
                        ));
                    }
                    for offset in 1..=3 {
                        if idx + offset < lines.len() {
                            file_block.push_str(&format!(
                                " {}:{}\n",
                                line_no + offset,
                                truncate(lines[idx + offset], MAX_LINE_COLUMNS)
                            ));
                        }
                    }
                }
            }

            if file_matches > 0 {
                output.push_str(&file_block);
            }
        }

        Ok(json!({
            "content": [{
                "type": "text",
                "text": output
            }]
        }))
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Respect char boundaries.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}
