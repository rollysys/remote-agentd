//! remote_read tool — read a file or directory on the local filesystem.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use hashline::{compute_file_hash, format_hashline_header};

/// Default line cap for full-file reads.
const DEFAULT_LINE_CAP: usize = 3000;

/// Tool implementing file/directory reads.
pub struct ReadTool;

impl ReadTool {
    /// Execute the read tool.
    ///
    /// Input: `{ path: string }` — path may include `:selector`
    /// (e.g. `:50-100`, `:raw`, `:50+10`).
    pub fn execute(args: &Value) -> Result<Value> {
        let raw_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`path` is required and must be a string"))?;
        let sudo = args.get("sudo").and_then(|v| v.as_bool()).unwrap_or(false);

        let (path_str, selector) = parse_selector(raw_path);
        let path = Path::new(&path_str);

        if sudo {
            // sudo path: use sudo-aware stat + read to handle root-owned files.
            let meta = crate::tools::sudo::file_metadata_sudo(path)
                .map_err(|e| anyhow!("Cannot stat {}: {}", path_str, e))?;
            if meta.is_dir {
                return Self::read_directory(&path_str);
            }
            return Self::read_file_sudo(&path_str, selector);
        }

        if !path.exists() {
            return Err(anyhow!("File not found: {}", path_str));
        }

        if path.is_dir() {
            return Self::read_directory(&path_str);
        }

        Self::read_file(&path_str, selector)
    }

    /// Read a directory and emit a depth-2 tree (dirs first, then files).
    fn read_directory(path_str: &str) -> Result<Value> {
        let path = Path::new(path_str);
        let mut dirs: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();

        for entry in fs::read_dir(path).map_err(|e| anyhow!("Failed to read dir {}: {}", path_str, e))? {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry
                .file_type()
                .map(|ft| ft.is_dir())
                .unwrap_or(false);
            if is_dir {
                dirs.push(format!("{}/", name));
            } else {
                files.push(name);
            }
        }
        dirs.sort();
        files.sort();

        let mut output = String::new();
        for d in &dirs {
            output.push_str(d);
            output.push('\n');
        }
        for f in &files {
            output.push_str(f);
            output.push('\n');
        }

        Ok(json!({
            "content": [{
                "type": "text",
                "text": output
            }]
        }))
    }

    /// Read a file applying the parsed selector.
    fn read_file(path_str: &str, selector: Option<Selector>) -> Result<Value> {
        let content = fs::read_to_string(Path::new(path_str))
            .map_err(|e| anyhow!("Failed to read {}: {}", path_str, e))?;
        Self::format_file_content(path_str, &content, selector)
    }

    /// sudo variant: reads file via `sudo -n cat`.
    fn read_file_sudo(path_str: &str, selector: Option<Selector>) -> Result<Value> {
        let content = crate::tools::sudo::read_file(Path::new(path_str), true)
            .map_err(|e| anyhow!("Failed to sudo-read {}: {}", path_str, e))?;
        Self::format_file_content(path_str, &content, selector)
    }

    /// Format file content with optional selector (shared by sudo/non-sudo paths).
    fn format_file_content(path_str: &str, content: &str, selector: Option<Selector>) -> Result<Value> {

        // Raw mode: no header, no line numbers.
        if matches!(selector, Some(Selector::Raw)) {
            return Ok(json!({
                "content": [{
                    "type": "text",
                    "text": content
                }]
            }));
        }

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let tag = compute_file_hash(&content);
        let header = format_hashline_header(path_str, &tag);

        let (start, end, is_range) = match selector {
            Some(Selector::Range { start, end }) => {
                let s = start.max(1) as usize;
                let e = (end as usize).min(total).max(s);
                (s, e, true)
            }
            Some(Selector::FromOffset { start, count }) => {
                let s = start.max(1) as usize;
                let e = ((s + count as usize - 1).min(total)).max(s);
                (s, e, true)
            }
            None => (1usize, total.min(DEFAULT_LINE_CAP), false),
            // Unreachable: Raw is handled by the early return above.
            Some(Selector::Raw) => (1usize, total.min(DEFAULT_LINE_CAP), false),
        };

        // For ranges, add 1 leading context line and 3 trailing context lines,
        // clamped to file bounds.
        let (disp_start, disp_end) = if is_range {
            let ds = start.saturating_sub(1).max(1);
            let de = (end + 3).min(total);
            (ds, de)
        } else {
            (start, end)
        };

        let mut output = String::new();
        output.push_str(&header);
        output.push('\n');

        for (i, line) in lines.iter().enumerate() {
            let line_no = i + 1;
            if line_no < disp_start || line_no > disp_end {
                continue;
            }
            output.push_str(&format!("{}:{}\n", line_no, line));
        }

        // Footer when truncated.
        if is_range && (disp_end < total || disp_start > 1) {
            output.push_str(&format!(
                "…\n[showing lines {}-{} of {}]\n",
                disp_start, disp_end, total
            ));
        } else if matches!(selector, None) && total > DEFAULT_LINE_CAP {
            output.push_str(&format!(
                "…\n[{} lines total — showing first {}; use :N-M to select a range]\n",
                total, DEFAULT_LINE_CAP
            ));
        }

        Ok(json!({
            "content": [{
                "type": "text",
                "text": output
            }]
        }))
    }
}

/// A parsed path selector.
#[derive(Debug, Clone, PartialEq)]
enum Selector {
    Raw,
    /// `:N-M` inclusive range.
    Range { start: u32, end: u32 },
    /// `:N+COUNT` — COUNT lines starting at N.
    FromOffset { start: u32, count: u32 },
}

/// Split a path from its trailing `:selector`, returning (path, selector).
///
/// The selector must match one of: `raw`, `N-M`, `N+M`, `N`, `N-M,M2` (multi-range
/// collapsed to Range with leading range), `conflicts`. A bare `host:port` style
/// suffix is not treated as a selector.
fn parse_selector(raw: &str) -> (String, Option<Selector>) {
    // Find the last ':' that begins a selector.
    // A selector ':' must be followed by a selector pattern.
    // Walk from the right; but only accept if the part after ':' looks like a
    // selector and the part before doesn't look like a Windows drive / scheme.
    let bytes = raw.as_bytes();
    let mut colon_idx = None;
    for (i, &b) in bytes.iter().enumerate().rev() {
        if b == b':' {
            let after = &raw[i + 1..];
            if looks_like_selector(after) {
                // Reject scheme-like `http:` — the segment before ':' would be
                // a scheme only if it's the FIRST colon and there's a `//`.
                // We accept the last selector-looking colon regardless.
                colon_idx = Some(i);
                break;
            }
        }
    }

    let Some(idx) = colon_idx else {
        return (raw.to_string(), None);
    };

    let path = raw[..idx].to_string();
    let sel = &raw[idx + 1..];

    let selector = parse_selector_token(sel);
    (path, selector)
}

/// Does the text after a ':' look like a selector?
fn looks_like_selector(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s == "raw" || s == "conflicts" {
        return true;
    }
    // Multi-range like `50-100,200-300`: accept if every comma-segment is a
    // range/offset/anchor pattern.
    s.split(',').all(seg_looks_like_selector)
}

fn seg_looks_like_selector(seg: &str) -> bool {
    let seg = seg.trim();
    if seg.is_empty() {
        return false;
    }
    // Anchor form `N` (single number), `N-M`, `N+M`, `N-M:raw`, `raw:N-M`.
    // Simplify: strip a trailing or leading `:raw`/`raw:` and re-check.
    let core = seg
        .strip_prefix("raw:")
        .or_else(|| seg.strip_suffix(":raw"))
        .unwrap_or(seg);
    // Now core should be a number, N-M, or N+M.
    parse_numeric_selector(core).is_some()
}

fn parse_numeric_selector(s: &str) -> Option<Selector> {
    let s = s.trim();
    // N+M
    if let Some(plus) = s.find('+') {
        let start: u32 = s[..plus].parse().ok()?;
        let count: u32 = s[plus + 1..].parse().ok()?;
        return Some(Selector::FromOffset { start, count });
    }
    // N-M
    if let Some(dash) = s.find('-') {
        let start: u32 = s[..dash].parse().ok()?;
        let end: u32 = s[dash + 1..].parse().ok()?;
        return Some(Selector::Range { start, end });
    }
    // N — single line
    let n: u32 = s.parse().ok()?;
    Some(Selector::Range { start: n, end: n })
}

fn parse_selector_token(s: &str) -> Option<Selector> {
    if s == "raw" {
        return Some(Selector::Raw);
    }
    if s == "conflicts" {
        // Treat as raw — conflict listing is a git concern we expose verbatim.
        return Some(Selector::Raw);
    }
    // Multi-range: take the first segment.
    let first = s.split(',').next()?;
    let core = first
        .strip_prefix("raw:")
        .or_else(|| first.strip_suffix(":raw"))
        .unwrap_or(first);
    parse_numeric_selector(core)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentd_read_test_{}_{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn selector_parsing() {
        assert_eq!(parse_selector("foo.rs"), ("foo.rs".into(), None));
        let (p, s) = parse_selector("foo.rs:raw");
        assert_eq!(p, "foo.rs");
        assert!(matches!(s, Some(Selector::Raw)));
        let (p, s) = parse_selector("foo.rs:2-4");
        assert_eq!(p, "foo.rs");
        assert!(matches!(s, Some(Selector::Range { start: 2, end: 4 })));
        let (p, s) = parse_selector("foo.rs:1+3");
        assert_eq!(p, "foo.rs");
        assert!(matches!(s, Some(Selector::FromOffset { start: 1, count: 3 })));
        let (p, s) = parse_selector("foo.rs:5");
        assert_eq!(p, "foo.rs");
        assert!(matches!(s, Some(Selector::Range { start: 5, end: 5 })));
    }

    #[test]
    fn read_full_file() {
        let d = tmp("full");
        let f = d.join("a.txt");
        fs::write(&f, "alpha\nbeta\ngamma\n").unwrap();
        let args = json!({ "path": f.to_str().unwrap() });
        let res = ReadTool::execute(&args).unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("a.txt#"), "header present");
        assert!(text.contains("1:alpha"));
        assert!(text.contains("2:beta"));
        assert!(text.contains("3:gamma"));
    }

    #[test]
    fn read_range_with_context() {
        let d = tmp("range");
        let f = d.join("b.txt");
        fs::write(&f, "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n").unwrap();
        let args = json!({ "path": format!("{}:3-4", f.to_str().unwrap()) });
        let res = ReadTool::execute(&args).unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        // range 3-4: 1 leading context (l2) + 3 trailing context (l5,l6,l7)
        assert!(text.contains("2:l2"), "leading context");
        assert!(text.contains("3:l3"));
        assert!(text.contains("4:l4"));
        assert!(text.contains("5:l5"), "trailing context 1");
        assert!(text.contains("6:l6"), "trailing context 2");
        assert!(text.contains("7:l7"), "trailing context 3");
        assert!(!text.contains("1:l1"), "out of range should not show");
        assert!(!text.contains("8:l8"), "out of range should not show");
    }

    #[test]
    fn read_raw_mode() {
        let d = tmp("raw");
        let f = d.join("c.txt");
        fs::write(&f, "x\ny\n").unwrap();
        let args = json!({ "path": format!("{}:raw", f.to_str().unwrap()) });
        let res = ReadTool::execute(&args).unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains('#'), "no header in raw");
        assert_eq!(text, "x\ny\n");
    }

    #[test]
    fn read_offset_selector() {
        let d = tmp("offset");
        let f = d.join("d.txt");
        fs::write(&f, "a\nb\nc\nd\ne\n").unwrap();
        let args = json!({ "path": format!("{}:2+2", f.to_str().unwrap()) });
        let res = ReadTool::execute(&args).unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("2:b"));
        assert!(text.contains("3:c"));
    }

    #[test]
    fn read_directory() {
        let d = tmp("dir");
        let sub = d.join("subdir");
        fs::create_dir_all(&sub).unwrap();
        fs::write(d.join("file1.txt"), "hi").unwrap();
        fs::write(d.join("file2.rs"), "yo").unwrap();
        let args = json!({ "path": d.to_str().unwrap() });
        let res = ReadTool::execute(&args).unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("subdir/"), "dirs suffixed with /");
        assert!(text.contains("file1.txt"));
        assert!(text.contains("file2.rs"));
    }

    #[test]
    fn read_missing_errors() {
        let args = json!({ "path": "/no/such/path/xyz123" });
        assert!(ReadTool::execute(&args).is_err());
    }
}
