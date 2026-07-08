//! Prefixes — port of packages/hashline/src/prefixes.ts
//!
//! Helpers that detect and strip hashline line-number prefixes (`123:`),
//! diff-style `+` prefixes, and read-output truncation notices from payloads
//! authored against `read`/`search` output. Two strip modes:
//!
//! - `strip_new_line_prefixes` — opportunistic: strips when the input clearly
//!   carries hashline or diff prefixes, leaves it alone otherwise.
//! - `strip_hashline_prefixes` — strict: only strips when every non-empty
//!   content line is hashline-prefixed.
//!
//! These run *before* the tokenizer; they exist because hashline mode is the
//! common case for echoed file content, and erroneously echoed prefixes will
//! otherwise turn every content line into a (malformed) op.

use std::sync::LazyLock;

use crate::format::HL_FILE_HASH_LENGTH;

// ── Regexes (mirrors the TS module-level consts) ──────────────────────────

/// `^\s*(?:>>>|>>)?\s*(?:[+*-]\s*)?\d+:`
const HL_PREFIX_PATTERN: &str = r"^\s*(?:>>>|>>)?\s*(?:[+*-]\s*)?\d+:";
/// `^\s*(?:>>>|>>)?\s*\+\s*\d+:`
const HL_PREFIX_PLUS_PATTERN: &str = r"^\s*(?:>>>|>>)?\s*\+\s*\d+:";
/// Read/search truncation notice: `[Showing lines …]` or `[N more lines in …]`.
const READ_TRUNCATION_NOTICE_PATTERN: &str =
    r"^\[(?:Showing lines \d+-\d+ of \d+|\d+ more lines? in (?:file|\S+))\b.*\bUse :L?\d+";

static HL_PREFIX_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(HL_PREFIX_PATTERN).unwrap());
static HL_PREFIX_PLUS_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(HL_PREFIX_PLUS_PATTERN).unwrap());
static HL_HEADER_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    let pat = format!(
        r"^\s*\[[^#\r\n]+#[0-9a-fA-F]{{{}}}\]\s*$",
        HL_FILE_HASH_LENGTH
    );
    regex::Regex::new(&pat).unwrap()
});
static READ_TRUNCATION_NOTICE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(READ_TRUNCATION_NOTICE_PATTERN).unwrap());

// ── Internal helpers ──────────────────────────────────────────────────────

/// Repeatedly strip leading hashline prefixes until none remain (TS
/// `stripLeadingHashlinePrefixes`).
fn strip_leading_hashline_prefixes(line: &str) -> String {
    let mut result = line.to_string();
    loop {
        let new = HL_PREFIX_RE.replace(&result, "").into_owned();
        if new == result {
            return result;
        }
        result = new;
    }
}

/// Strip a single leading `+` that is NOT followed by another `+`.
/// Mirrors TS `line.replace(/^[+](?![+])/, "")`. Rust's regex crate lacks
/// look-ahead, so we implement it manually.
fn strip_diff_plus_prefix(line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.first() == Some(&b'+') && bytes.get(1) != Some(&b'+') {
        return line[1..].to_string();
    }
    line.to_string()
}

/// Test whether a line starts with `+` not followed by another `+`.
fn is_diff_plus(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.first() == Some(&b'+') && bytes.get(1) != Some(&b'+')
}

/// Single-pass variant: strip at most one leading hashline prefix (`N:`,
/// `>>>N:`, `+N:` etc.) and do NOT recurse. Matches TS
/// `stripOneLeadingHashlinePrefix`.
pub fn strip_one_leading_hashline_prefix(line: &str) -> String {
    HL_PREFIX_RE.replace(line, "").into_owned()
}

#[derive(Debug, Default, Clone, Copy)]
struct LinePrefixStats {
    non_empty: usize,
    header_count: usize,
    hash_prefix_count: usize,
    diff_plus_hash_prefix_count: usize,
    diff_plus_count: usize,
    truncation_notice_count: usize,
}

fn collect_line_prefix_stats(lines: &[String]) -> LinePrefixStats {
    let mut stats = LinePrefixStats::default();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if READ_TRUNCATION_NOTICE_RE.is_match(line) {
            stats.truncation_notice_count += 1;
            continue;
        }
        if HL_HEADER_RE.is_match(line) {
            stats.non_empty += 1;
            stats.header_count += 1;
            continue;
        }
        stats.non_empty += 1;
        if HL_PREFIX_RE.is_match(line) {
            stats.hash_prefix_count += 1;
        }
        if HL_PREFIX_PLUS_RE.is_match(line) {
            stats.diff_plus_hash_prefix_count += 1;
        }
        if is_diff_plus(line) {
            stats.diff_plus_count += 1;
        }
    }
    stats
}

// ── Public API ────────────────────────────────────────────────────────────

/// Strip whichever prefix scheme the lines appear to be carrying:
/// - hashline line-number prefixes (`123:`) when every content line has one
/// - leading `+` (diff style) when at least half the lines have one
/// - mixed `+<n>:` form when present
///
/// Returns the lines untouched if no scheme is recognized.
pub fn strip_new_line_prefixes(lines: &[String]) -> Vec<String> {
    let stats = collect_line_prefix_stats(lines);
    if stats.non_empty == 0 {
        return lines.to_vec();
    }

    let content_line_count = stats.non_empty - stats.header_count;
    let strip_hash = content_line_count > 0 && stats.hash_prefix_count == content_line_count;
    let strip_plus = !strip_hash
        && stats.diff_plus_hash_prefix_count == 0
        && stats.diff_plus_count > 0
        && (stats.diff_plus_count as f64) >= (stats.non_empty as f64) * 0.5;

    if !strip_hash && !strip_plus && stats.diff_plus_hash_prefix_count == 0 {
        return lines.to_vec();
    }

    lines
        .iter()
        .filter(|line| {
            !READ_TRUNCATION_NOTICE_RE.is_match(line) && !(strip_hash && HL_HEADER_RE.is_match(line))
        })
        .map(|line| {
            if strip_hash {
                strip_leading_hashline_prefixes(line)
            } else if strip_plus {
                strip_diff_plus_prefix(line)
            } else if stats.diff_plus_hash_prefix_count > 0 && HL_PREFIX_PLUS_RE.is_match(line) {
                HL_PREFIX_RE.replace(line, "").into_owned()
            } else {
                line.clone()
            }
        })
        .collect()
}

/// Strict variant: strip hashline prefixes only when every content line is
/// hashline-prefixed. Returns the lines unchanged otherwise.
pub fn strip_hashline_prefixes(lines: &[String]) -> Vec<String> {
    let stats = collect_line_prefix_stats(lines);
    if stats.non_empty == 0 {
        return lines.to_vec();
    }
    let content_line_count = stats.non_empty - stats.header_count;
    if content_line_count == 0 || stats.hash_prefix_count != content_line_count {
        return lines.to_vec();
    }
    lines
        .iter()
        .filter(|line| !READ_TRUNCATION_NOTICE_RE.is_match(line) && !HL_HEADER_RE.is_match(line))
        .map(|line| strip_leading_hashline_prefixes(line))
        .collect()
}

/// Normalize line payloads by stripping read/search line prefixes. A single
/// multiline string is split on `\n` (trailing newline trimmed first; `\r`
/// removed). Empty input yields `[]`.
pub fn hashline_parse_text(edit: &str) -> Vec<String> {
    if edit.is_empty() {
        return Vec::new();
    }
    let trimmed = if edit.ends_with('\n') {
        &edit[..edit.len() - 1]
    } else {
        edit
    };
    let without_cr = trimmed.replace('\r', "");
    let lines: Vec<String> = without_cr.split('\n').map(|s| s.to_string()).collect();
    strip_new_line_prefixes(&lines)
}

/// Variant accepting pre-split lines (matches the TS `string[]` overload).
pub fn hashline_parse_text_lines(edit: &[String]) -> Vec<String> {
    strip_new_line_prefixes(edit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_one_leading_prefix_basic() {
        assert_eq!(strip_one_leading_hashline_prefix("123:hello"), "hello");
        assert_eq!(strip_one_leading_hashline_prefix("42:hello"), "hello");
        assert_eq!(strip_one_leading_hashline_prefix("hello"), "hello");
        // Single-pass: does not recurse on "2:42:hello"
        assert_eq!(strip_one_leading_hashline_prefix("2:42:hello"), "42:hello");
        assert_eq!(strip_one_leading_hashline_prefix("+3:keep"), "keep");
    }

    #[test]
    fn strip_new_line_prefixes_hashlines() {
        let lines = vec![
            "1:aaa".to_string(),
            "2:bbb".to_string(),
            "3:ccc".to_string(),
        ];
        assert_eq!(
            strip_new_line_prefixes(&lines),
            vec!["aaa", "bbb", "ccc"]
        );
    }

    #[test]
    fn strip_new_line_prefixes_leaves_unprefixed() {
        let lines = vec![
            "aaa".to_string(),
            "bbb".to_string(),
            "ccc".to_string(),
        ];
        assert_eq!(strip_new_line_prefixes(&lines), lines);
    }

    #[test]
    fn strip_new_line_prefixes_diff_plus() {
        let lines = vec![
            "+aaa".to_string(),
            "+bbb".to_string(),
            "+ccc".to_string(),
        ];
        assert_eq!(
            strip_new_line_prefixes(&lines),
            vec!["aaa", "bbb", "ccc"]
        );
    }

    #[test]
    fn strip_hashline_prefixes_strict() {
        let lines = vec![
            "1:aaa".to_string(),
            "2:bbb".to_string(),
            "3:ccc".to_string(),
        ];
        assert_eq!(
            strip_hashline_prefixes(&lines),
            vec!["aaa", "bbb", "ccc"]
        );
        // Not all prefixed → unchanged
        let mixed = vec![
            "1:aaa".to_string(),
            "bbb".to_string(),
        ];
        assert_eq!(strip_hashline_prefixes(&mixed), mixed);
    }

    #[test]
    fn hashline_parse_text_string() {
        assert_eq!(hashline_parse_text("1:aaa\n2:bbb"), vec!["aaa", "bbb"]);
        assert_eq!(hashline_parse_text("aaa\nbbb"), vec!["aaa", "bbb"]);
        // Trailing newline trimmed
        assert_eq!(hashline_parse_text("aaa\nbbb\n"), vec!["aaa", "bbb"]);
        assert_eq!(hashline_parse_text(""), Vec::<String>::new());
    }

    #[test]
    fn hashline_parse_text_drops_header_and_truncation() {
        let input = "[src/foo.ts#1A2B]\n1:aaa\n[Showing lines 1-1 of 1. Use :L1]\n2:bbb";
        assert_eq!(hashline_parse_text(input), vec!["aaa", "bbb"]);
    }
}
