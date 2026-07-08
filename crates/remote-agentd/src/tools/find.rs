//! remote_find tool — find files matching paths/globs, sorted by mtime.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use ignore::WalkBuilder;
use serde_json::{json, Value};

/// Tool implementing file discovery via directory walking.
pub struct FindTool;

impl FindTool {
    /// Execute the find tool.
    ///
    /// Input: `{ paths: array, gitignore?: bool, hidden?: bool }`
    /// Walks each path with the `ignore` crate, returns files sorted by mtime
    /// (newest first), grouped under `# dir/` headers.
    pub fn execute(args: &Value) -> Result<Value> {
        let paths_in: Vec<String> = args
            .get("paths")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("`paths` is required and must be an array"))?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        if paths_in.is_empty() {
            return Err(anyhow!("`paths` must contain at least one path"));
        }

        let gitignore = args.get("gitignore").and_then(|v| v.as_bool()).unwrap_or(true);
        let hidden = args.get("hidden").and_then(|v| v.as_bool()).unwrap_or(true);

        let mut entries: Vec<(PathBuf, SystemTime)> = Vec::new();

        for root in &paths_in {
            let root_path = Path::new(root);
            let walker = WalkBuilder::new(root_path)
                .hidden(!hidden)
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
                if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                    continue;
                }
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                entries.push((entry.into_path(), mtime));
            }
        }

        // Sort by mtime descending (newest first).
        entries.sort_by(|a, b| b.1.cmp(&a.1));

        // Group by directory, emitting `# dir/` headers.
        let mut output = String::new();
        let mut current_dir: Option<PathBuf> = None;
        for (path, _mtime) in &entries {
            let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            if current_dir.as_ref() != Some(&parent) {
                if !output.is_empty() {
                    output.push('\n');
                }
                let dir_display = if parent.as_os_str().is_empty() {
                    ".".to_string()
                } else {
                    parent.display().to_string()
                };
                output.push_str(&format!("# {}/\n", dir_display));
                current_dir = Some(parent);
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            output.push_str(&name);
            output.push('\n');
        }

        Ok(json!({
            "content": [{
                "type": "text",
                "text": output
            }]
        }))
    }
}
