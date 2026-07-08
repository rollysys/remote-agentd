//! Diff preview — port of packages/hashline/src/diff-preview.ts
//!
//! Builds a compact numbered-diff preview and a unified diff.

use similar::{ChangeTag, TextDiff};

/// Re-number a unified diff that uses the `+<lineNum>|content` /
/// `-<lineNum>|content` / ` <lineNum>|content` line format into a compact
/// current-file preview.

const DEFAULT_ADDED_RUN_CONTEXT_LINES: usize = 2;
const PREVIEW_ELISION_MARKER: &str = "…";
const PREVIEW_GAP_ROW: &str = "";

/// Result of `build_compact_diff_preview`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactDiffPreview {
    pub preview: String,
    pub added_lines: u32,
    pub removed_lines: u32,
}

/// Optional knobs for `build_compact_diff_preview`.
#[derive(Debug, Clone, Default)]
pub struct CompactDiffOptions {
    /// Added lines kept on each side of a long added-run elision (default 2).
    pub max_added_run_context: Option<usize>,
    /// Back-compat alias for `max_added_run_context`.
    pub max_unchanged_run: Option<usize>,
}

fn normalize_added_run_context(value: Option<usize>) -> usize {
    value.unwrap_or(DEFAULT_ADDED_RUN_CONTEXT_LINES).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DiffKind {
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone)]
struct ParsedDiffLine {
    kind: DiffKind,
    line_number: u32,
    content: String,
}

fn parse_numbered_diff_line(line: &str) -> Option<ParsedDiffLine> {
    let kind = match line.chars().next()? {
        '+' => DiffKind::Added,
        '-' => DiffKind::Removed,
        ' ' => DiffKind::Context,
        _ => return None,
    };
    let body = &line[1..];
    let sep = body.find('|')?;
    let line_number: u32 = body[..sep].parse().ok()?;
    let content = body[sep + 1..].to_string();
    Some(ParsedDiffLine { kind, line_number, content })
}

fn is_raw_elision(line: &str) -> bool {
    line == "..." || line == PREVIEW_ELISION_MARKER || line == &format!("+{PREVIEW_ELISION_MARKER}")
}

fn is_preview_separator(line: &str) -> bool {
    line == PREVIEW_ELISION_MARKER || line == PREVIEW_GAP_ROW
}

fn append_preview_line(output: &mut Vec<String>, line: &str) {
    let normalized = if is_raw_elision(line) {
        PREVIEW_ELISION_MARKER
    } else {
        line
    };
    // Separators never stack; a leading separator is dropped outright.
    if is_preview_separator(normalized)
        && (output.is_empty() || is_preview_separator(output.last().unwrap()))
    {
        return;
    }
    output.push(normalized.to_string());
}

fn append_added_run(output: &mut Vec<String>, run: &[String], edge_lines: usize) {
    if run.is_empty() {
        return;
    }
    let collapse_threshold = edge_lines * 2 + 1;
    if run.len() <= collapse_threshold {
        for text in run {
            append_preview_line(output, text);
        }
        return;
    }
    for i in 0..edge_lines {
        append_preview_line(output, &run[i]);
    }
    append_preview_line(output, PREVIEW_ELISION_MARKER);
    for i in (run.len() - edge_lines)..run.len() {
        append_preview_line(output, &run[i]);
    }
}

/// Build a compact diff preview from a numbered diff string.
pub fn build_compact_diff_preview(diff: &str, options: &CompactDiffOptions) -> CompactDiffPreview {
    let lines: Vec<&str> = if diff.is_empty() { Vec::new() } else { diff.split('\n').collect() };
    let added_run_context = normalize_added_run_context(
        options
            .max_added_run_context
            .or(options.max_unchanged_run),
    );
    let mut added_lines: u32 = 0;
    let mut removed_lines: u32 = 0;
    let mut formatted: Vec<String> = Vec::new();
    let mut added_run: Vec<String> = Vec::new();

    let flush_added_run = |formatted: &mut Vec<String>, added_run: &mut Vec<String>| {
        append_added_run(formatted, added_run, added_run_context);
        added_run.clear();
    };

    for line in &lines {
        match parse_numbered_diff_line(line) {
            Some(parsed) => match parsed.kind {
                DiffKind::Added => {
                    added_lines += 1;
                    added_run.push(format!("{}:{}", parsed.line_number, parsed.content));
                }
                DiffKind::Removed => {
                    flush_added_run(&mut formatted, &mut added_run);
                    removed_lines += 1;
                }
                DiffKind::Context => {
                    flush_added_run(&mut formatted, &mut added_run);
                    let new_line_number =
                        parsed.line_number as i64 + added_lines as i64 - removed_lines as i64;
                    if new_line_number >= 0 {
                        append_preview_line(
                            &mut formatted,
                            &format!("{}:{}", new_line_number, parsed.content),
                        );
                    }
                }
            },
            None => {
                flush_added_run(&mut formatted, &mut added_run);
                append_preview_line(&mut formatted, line);
            }
        }
    }
    flush_added_run(&mut formatted, &mut added_run);

    while let Some(last) = formatted.last() {
        if is_preview_separator(last) {
            formatted.pop();
        } else {
            break;
        }
    }

    CompactDiffPreview {
        preview: formatted.join("\n"),
        added_lines,
        removed_lines,
    }
}

/// Produce a unified diff (with line numbers) between `old_text` and `new_text`.
///
/// Each diff line is prefixed with `+`, `-`, or ` ` and carries the post-edit
/// (for `+`) or pre-edit (for `-` and ` `) 1-indexed line number followed by
/// `|` and the content:
///   `+5|new line`
///   `-4|old line`
///   ` 3|context`
pub fn make_diff_preview(old_text: &str, new_text: &str) -> String {
    let old_lines: Vec<&str> = if old_text.is_empty() {
        Vec::new()
    } else {
        old_text.split('\n').collect()
    };
    let new_lines: Vec<&str> = if new_text.is_empty() {
        Vec::new()
    } else {
        new_text.split('\n').collect()
    };

    let diff = TextDiff::from_slices(&old_lines, &new_lines);
    let mut output: Vec<String> = Vec::new();
    let mut old_lineno: u32 = 0;
    let mut new_lineno: u32 = 0;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                old_lineno += 1;
                new_lineno += 1;
                output.push(format!(" {old_lineno}|{}", change.value()));
            }
            ChangeTag::Delete => {
                old_lineno += 1;
                output.push(format!("-{old_lineno}|{}", change.value()));
            }
            ChangeTag::Insert => {
                new_lineno += 1;
                output.push(format!("+{new_lineno}|{}", change.value()));
            }
        }
    }
    output.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_inputs_produce_empty_preview() {
        let result = build_compact_diff_preview("", &CompactDiffOptions::default());
        assert_eq!(result.preview, "");
        assert_eq!(result.added_lines, 0);
        assert_eq!(result.removed_lines, 0);
    }

    #[test]
    fn make_diff_basic() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let diff = make_diff_preview(old, new);
        assert!(diff.contains("-2|b"));
        assert!(diff.contains("+2|B"));
        assert!(diff.contains(" 1|a"));
        assert!(diff.contains(" 3|c"));
    }
}
