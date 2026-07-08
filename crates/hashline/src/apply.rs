//! Apply edits — port of packages/hashline/src/apply.ts
//!
//! Apply a parsed list of [`Edit`]s to a text body and return the post-edit
//! lines plus any diagnostic warnings. Pure function: no FS, no mutation of
//! the input.
//!
//! Replacement groups are first normalized by `repair_replacement_boundaries`,
//! which absorbs common model mistakes where a payload restates unchanged range
//! boundaries or duplicates/drops structural closers. After that,
//! `repair_after_insert_landings` slides mis-anchored after-insert hunks to
//! the depth their body indentation claims.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::types::{Anchor, ApplyResult, Cursor, Edit, InsertMode};

// ── Line origin tracking (matches TS `LineOrigin`) ────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineOrigin {
    Original,
    Insert,
    Replacement,
}

// ── Indexed edit wrapper ──────────────────────────────────────────────────
//
// The TS Edit carries `lineNum` (the source patch line of the hunk) and
// `index` (a monotonic counter across all edits). The Rust `Edit` enum does
// not carry these, so we reconstruct them:
//   - `idx` is the position in the input slice (stable, monotonic).
//   - `block_start` is the resolved block's first line for INS.BLK.POST
//     lowerings; `None` for all other edits. Callers pass it via
//     `ApplyOptions`.
//   - `hunk_id` groups rows that came from the same authored hunk. For
//     replacement inserts this groups a `SWAP N.=M:` body + its contiguous
//     deletes; for plain inserts it groups rows sharing the same anchor +
//     cursor kind. We derive it structurally (see `build_indexed_edits`).

#[derive(Debug, Clone)]
struct IndexedEdit {
    edit: Edit,
    #[allow(dead_code)]
    idx: usize,
    block_start: Option<u32>,
    /// Identifier grouping rows from the same authored hunk. For replacement
    /// inserts this is the anchor line; for plain after-inserts it is the
    /// anchor line; for deletes it is the anchor line. Used only to keep
    /// consecutive replacement-insert rows together as one group.
    hunk_id: u32,
}

/// Optional metadata the caller supplies when block edits have been resolved.
#[derive(Debug, Default, Clone)]
pub struct ApplyOptions {
    /// Map from edit index (position in the `edits` slice) to the resolved
    /// block's first line, for inserts lowered from `INS.BLK.POST N:`. The
    /// applier uses this to apply the inward landing shift.
    pub block_starts: HashMap<usize, u32>,
}

// ── Regexes ───────────────────────────────────────────────────────────────

/// A line that is nothing but closing delimiters: `}`, `)`, `];`, `})`, `},`.
const STRUCTURAL_CLOSER_PATTERN: &str = r"^\s*[)\]}]+[;,]?\s*$";
/// A JSX/XML closing boundary.
const JSX_CLOSER_PATTERN: &str = r"^\s*(?:</>|</[A-Za-z][\w.:-]*>|/>)\s*[;,]?\s*$";
const JSX_NAMED_CLOSER_PATTERN: &str = r"^\s*</([A-Za-z][\w.:-]*)>\s*[;,]?\s*$";
const JSX_FRAGMENT_CLOSER_PATTERN: &str = r"^\s*</>\s*[;,]?\s*$";

static STRUCTURAL_CLOSER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(STRUCTURAL_CLOSER_PATTERN).unwrap());
static JSX_CLOSER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(JSX_CLOSER_PATTERN).unwrap());
static JSX_NAMED_CLOSER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(JSX_NAMED_CLOSER_PATTERN).unwrap());
static JSX_FRAGMENT_CLOSER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(JSX_FRAGMENT_CLOSER_PATTERN).unwrap());

fn is_structural_closer_line(text: &str) -> bool {
    STRUCTURAL_CLOSER_RE.is_match(text) || JSX_CLOSER_RE.is_match(text)
}

fn jsx_closer_name(text: &str) -> Option<String> {
    if JSX_FRAGMENT_CLOSER_RE.is_match(text) {
        return Some(String::new());
    }
    JSX_NAMED_CLOSER_RE
        .captures(text)
        .map(|c| c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default())
}

// ── JSX payload tag parsing ───────────────────────────────────────────────

#[derive(Debug, Clone)]
struct JsxPayloadTag {
    name: String,
    closing: bool,
    self_closing: bool,
}

fn is_jsx_tag_start(text: &str, index: usize) -> bool {
    let next = text.as_bytes().get(index + 1).copied();
    match next {
        Some(b'>') | Some(b'/') => true,
        Some(c) if c.is_ascii_alphabetic() => true,
        _ => false,
    }
}

fn find_jsx_tag_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut braces = 0i32;
    let mut i = start + 1;
    while i < bytes.len() {
        let ch = bytes[i];
        if let Some(q) = quote {
            if ch == b'\\' && i + 1 < bytes.len() {
                i += 1;
            } else if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match ch {
            b'"' | b'\'' | b'`' => {
                quote = Some(ch);
            }
            b'{' => braces += 1,
            b'}' if braces > 0 => braces -= 1,
            b'>' if braces == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_jsx_payload_tag(raw: &str) -> Option<JsxPayloadTag> {
    if raw == "<>" {
        return Some(JsxPayloadTag {
            name: String::new(),
            closing: false,
            self_closing: false,
        });
    }
    if raw == "</>" {
        return Some(JsxPayloadTag {
            name: String::new(),
            closing: true,
            self_closing: false,
        });
    }
    let closing = raw.starts_with("</");
    let name_start = if closing { 2 } else { 1 };
    let bytes = raw.as_bytes();
    let mut name_end = name_start;
    while name_end < bytes.len() {
        let c = bytes[name_end];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b':' || c == b'-' {
            name_end += 1;
        } else {
            break;
        }
    }
    if name_end == name_start {
        return None;
    }
    let name = raw[name_start..name_end].to_string();
    let self_closing = !closing && {
        let rest = &raw[name_end..];
        rest.ends_with("/>") && rest.trim_end().ends_with("/>")
    };
    Some(JsxPayloadTag {
        name,
        closing,
        self_closing,
    })
}

fn read_jsx_payload_tags(text: &str) -> Vec<JsxPayloadTag> {
    let mut tags = Vec::new();
    let bytes = text.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos] != b'<' {
            pos += 1;
            continue;
        }
        if !is_jsx_tag_start(text, pos) {
            pos += 1;
            continue;
        }
        match find_jsx_tag_end(text, pos) {
            Some(end) => {
                let raw = &text[pos..=end];
                if let Some(tag) = parse_jsx_payload_tag(raw) {
                    tags.push(tag);
                }
                pos = end + 1;
            }
            None => break,
        }
    }
    tags
}

fn payload_has_jsx_opener_for_echo(payload_prefix: &[String], echo_lines: &[String]) -> bool {
    let mut open_tags: Vec<String> = Vec::new();
    for tag in read_jsx_payload_tags(&payload_prefix.join("\n")) {
        if tag.closing {
            if open_tags.last().map(|n| n == &tag.name).unwrap_or(false) {
                open_tags.pop();
            }
        } else if !tag.self_closing {
            open_tags.push(tag.name.clone());
        }
    }
    for line in echo_lines {
        if let Some(name) = jsx_closer_name(line) {
            if open_tags.contains(&name) {
                return true;
            }
        }
    }
    false
}

// ── Delimiter balance ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DelimiterBalance {
    paren: i32,
    bracket: i32,
    brace: i32,
}

/// Net `()` / `[]` / `{}` delta across `lines`, skipping delimiters inside
/// line comments (`//`), block comments, and string/template literals.
fn compute_delimiter_balance(lines: &[String]) -> DelimiterBalance {
    let mut balance = DelimiterBalance::default();
    let mut in_block_comment = false;
    let mut quote: Option<u8> = None;
    for line in lines {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let ch = bytes[i];
            if in_block_comment {
                if ch == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    in_block_comment = false;
                    i += 1;
                }
                i += 1;
                continue;
            }
            if let Some(q) = quote {
                if ch == b'\\' {
                    i += 2;
                    continue;
                } else if ch == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            match ch {
                b'"' | b'\'' | b'`' => {
                    quote = Some(ch);
                }
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => break,
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                    in_block_comment = true;
                    i += 1;
                }
                b'(' => balance.paren += 1,
                b')' => balance.paren -= 1,
                b'[' => balance.bracket += 1,
                b']' => balance.bracket -= 1,
                b'{' => balance.brace += 1,
                b'}' => balance.brace -= 1,
                _ => {}
            }
            i += 1;
        }
        // `"` / `'` cannot span lines; only backtick templates and block comments do.
        if quote == Some(b'"') || quote == Some(b'\'') {
            quote = None;
        }
    }
    balance
}

fn balance_delta(a: DelimiterBalance, b: DelimiterBalance) -> DelimiterBalance {
    DelimiterBalance {
        paren: a.paren - b.paren,
        bracket: a.bracket - b.bracket,
        brace: a.brace - b.brace,
    }
}

fn balance_negate(a: DelimiterBalance) -> DelimiterBalance {
    DelimiterBalance {
        paren: -a.paren,
        bracket: -a.bracket,
        brace: -a.brace,
    }
}

fn balance_equal(a: DelimiterBalance, b: DelimiterBalance) -> bool {
    a.paren == b.paren && a.bracket == b.bracket && a.brace == b.brace
}

fn balance_is_zero(a: DelimiterBalance) -> bool {
    a.paren == 0 && a.bracket == 0 && a.brace == 0
}

fn balance_sum(a: DelimiterBalance, b: DelimiterBalance) -> DelimiterBalance {
    DelimiterBalance {
        paren: a.paren + b.paren,
        bracket: a.bracket + b.bracket,
        brace: a.brace + b.brace,
    }
}

fn balance_component_covers(candidate: i32, target: i32) -> bool {
    if target == 0 {
        return true;
    }
    (candidate > 0) == (target > 0) && candidate.abs() >= target.abs()
}

fn balance_covers(candidate: DelimiterBalance, target: DelimiterBalance) -> bool {
    balance_component_covers(candidate.paren, target.paren)
        && balance_component_covers(candidate.bracket, target.bracket)
        && balance_component_covers(candidate.brace, target.brace)
}

// ── Replacement group detection ───────────────────────────────────────────

#[derive(Debug, Clone)]
struct ReplacementGroup {
    /// Positions in the edit array of the payload inserts, in payload order.
    insert_indices: Vec<usize>,
    /// Positions in the edit array of the range deletes, ascending by line.
    delete_indices: Vec<usize>,
    payload: Vec<String>,
    /// First deleted line (1-indexed).
    start_line: u32,
    /// Last deleted line (1-indexed).
    end_line: u32,
}

fn is_replacement_insert(edit: &Edit) -> bool {
    matches!(
        edit,
        Edit::Insert {
            mode: InsertMode::Replacement,
            ..
        }
    )
}

fn cursor_anchor_line(cursor: &Cursor) -> Option<u32> {
    match cursor {
        Cursor::BeforeAnchor(a) | Cursor::AfterAnchor(a) => Some(a.line),
        Cursor::Bof | Cursor::Eof => None,
    }
}

fn get_cursor_anchors(cursor: &Cursor) -> Vec<Anchor> {
    match cursor {
        Cursor::BeforeAnchor(a) | Cursor::AfterAnchor(a) => vec![*a],
        Cursor::Bof | Cursor::Eof => vec![],
    }
}

fn get_edit_anchors(edit: &Edit) -> Vec<Anchor> {
    match edit {
        Edit::Delete { anchor } => vec![*anchor],
        Edit::Insert { cursor, .. } => get_cursor_anchors(cursor),
        Edit::Block { anchor, .. } => vec![*anchor],
    }
}

/// Detect a replacement group starting at `start`: a run of `before_anchor`
/// replacement inserts sharing one anchor, immediately followed by the
/// contiguous range deletes for that same op.
fn find_replacement_group(edits: &[IndexedEdit], start: usize) -> Option<ReplacementGroup> {
    let first = edits.get(start)?;
    let first_edit = &first.edit;
    if !is_replacement_insert(first_edit) {
        return None;
    }
    let anchor_line = match first_edit {
        Edit::Insert {
            cursor: Cursor::BeforeAnchor(a),
            ..
        } => a.line,
        _ => return None,
    };
    let first_hunk = first.hunk_id;

    let mut insert_indices = Vec::new();
    let mut payload: Vec<String> = Vec::new();
    let mut i = start;
    while i < edits.len() {
        let entry = &edits[i];
        if !is_replacement_insert(&entry.edit) || entry.hunk_id != first_hunk {
            break;
        }
        match &entry.edit {
            Edit::Insert {
                cursor: Cursor::BeforeAnchor(a),
                text,
                ..
            } if a.line == anchor_line => {
                insert_indices.push(i);
                payload.push(text.clone());
                i += 1;
            }
            _ => break,
        }
    }

    let mut delete_indices = Vec::new();
    let mut expected_line = anchor_line;
    while i < edits.len() {
        let entry = &edits[i];
        match &entry.edit {
            Edit::Delete { anchor } if entry.hunk_id == first_hunk && anchor.line == expected_line => {
                delete_indices.push(i);
                expected_line += 1;
                i += 1;
            }
            _ => break,
        }
    }

    if delete_indices.is_empty() {
        return None;
    }
    let delete_count = delete_indices.len() as u32;
    Some(ReplacementGroup {
        insert_indices,
        delete_indices,
        payload,
        start_line: anchor_line,
        end_line: anchor_line + delete_count - 1,
    })
}

// ── Duplicate prefix/suffix detection ─────────────────────────────────────

/// Largest `k` such that the payload's last `k` lines exactly equal the `k`
/// surviving file lines just below the range AND dropping them zeroes `delta`.
fn find_duplicate_suffix(
    group: &ReplacementGroup,
    file_lines: &[String],
    delta: DelimiterBalance,
) -> usize {
    if balance_is_zero(delta) {
        return 0;
    }
    let payload_len = group.payload.len();
    let max_k = payload_len.min(file_lines.len() - group.end_line as usize);
    for k in (1..=max_k).rev() {
        let mut matches = true;
        for t in 0..k {
            let pl = &payload_len - k + t;
            let fl = group.end_line as usize + t;
            if group.payload[pl] != file_lines[fl] {
                matches = false;
                break;
            }
        }
        if !matches {
            continue;
        }
        let suffix: Vec<String> = group.payload[payload_len - k..].to_vec();
        if balance_equal(compute_delimiter_balance(&suffix), delta) {
            return k;
        }
    }
    0
}

/// Largest `j` such that the payload's first `j` lines exactly equal the `j`
/// surviving file lines just above the range AND dropping them zeroes `delta`.
fn find_duplicate_prefix(
    group: &ReplacementGroup,
    file_lines: &[String],
    delta: DelimiterBalance,
) -> usize {
    if balance_is_zero(delta) {
        return 0;
    }
    let payload_len = group.payload.len();
    let max_j = payload_len.min(group.start_line as usize - 1);
    for j in (1..=max_j).rev() {
        let mut matches = true;
        for t in 0..j {
            let pl = t;
            let fl = group.start_line as usize - 1 - j + t;
            if group.payload[pl] != file_lines[fl] {
                matches = false;
                break;
            }
        }
        if !matches {
            continue;
        }
        let prefix: Vec<String> = group.payload[..j].to_vec();
        if balance_equal(compute_delimiter_balance(&prefix), delta) {
            return j;
        }
    }
    0
}

// ── Dropped suffix closers ────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DroppedSuffixClosers {
    start_line: u32,
    count: u32,
    balance: DelimiterBalance,
}

#[derive(Debug, Default, Clone)]
struct InsertedLineMaps {
    before: HashMap<u32, Vec<String>>,
    after: HashMap<u32, Vec<String>>,
}

fn count_payload_restated_suffix_head(payload: &[String], suffix_lines: &[String]) -> usize {
    let max_count = payload.len().min(suffix_lines.len());
    for count in (1..=max_count).rev() {
        let mut matches = true;
        for offset in 0..count {
            if payload[payload.len() - count + offset] != suffix_lines[offset] {
                matches = false;
                break;
            }
        }
        if matches {
            return count;
        }
    }
    0
}

fn count_projected_below_suffix_tail(
    group: &ReplacementGroup,
    file_lines: &[String],
    deleted_lines: &HashSet<u32>,
    inserted_line_maps: &InsertedLineMaps,
    suffix_lines: &[String],
) -> usize {
    let mut below: Vec<String> = Vec::new();
    let append_closer_lines =
        |lines: Option<&Vec<String>>, below: &mut Vec<String>| -> bool {
            let Some(lines) = lines else { return true };
            for text in lines {
                if !STRUCTURAL_CLOSER_RE.is_match(text) {
                    return false;
                }
                below.push(text.clone());
            }
            true
        };
    if !append_closer_lines(inserted_line_maps.after.get(&group.end_line), &mut below) {
        return 0;
    }
    let mut line = group.end_line + 1;
    while line as usize <= file_lines.len() {
        if !append_closer_lines(inserted_line_maps.before.get(&line), &mut below) {
            break;
        }
        if !deleted_lines.contains(&line) {
            let text = file_lines.get(line as usize - 1).map(|s| s.as_str()).unwrap_or("");
            if !STRUCTURAL_CLOSER_RE.is_match(text) {
                break;
            }
            below.push(text.to_string());
        }
        if !append_closer_lines(inserted_line_maps.after.get(&line), &mut below) {
            break;
        }
        line += 1;
    }
    let max_count = below.len().min(suffix_lines.len());
    for count in (1..=max_count).rev() {
        let mut matches = true;
        for offset in 0..count {
            if below[offset] != suffix_lines[suffix_lines.len() - count + offset] {
                matches = false;
                break;
            }
        }
        if matches {
            return count;
        }
    }
    0
}

fn compute_projected_prefix_balance(
    group: &ReplacementGroup,
    file_lines: &[String],
    deleted_lines: &HashSet<u32>,
    inserted_by_line: &HashMap<u32, Vec<String>>,
    inserted_line_maps: &InsertedLineMaps,
) -> DelimiterBalance {
    let mut prefix: Vec<String> = Vec::new();
    let mut line = 1u32;
    while line < group.start_line {
        if let Some(inserted) = inserted_by_line.get(&line) {
            prefix.extend(inserted.iter().cloned());
        }
        if !deleted_lines.contains(&line) {
            prefix.push(file_lines.get(line as usize - 1).cloned().unwrap_or_default());
        }
        line += 1;
    }
    if let Some(inserted_at_start) = inserted_line_maps.before.get(&group.start_line) {
        prefix.extend(inserted_at_start.iter().cloned());
    }
    prefix.extend(group.payload.iter().cloned());
    compute_delimiter_balance(&prefix)
}

fn prefix_can_cover_suffix_closers(
    group: &ReplacementGroup,
    file_lines: &[String],
    suffix_balance: DelimiterBalance,
    covered_below_balance: DelimiterBalance,
    deleted_lines: &HashSet<u32>,
    inserted_by_line: &HashMap<u32, Vec<String>>,
    inserted_line_maps: &InsertedLineMaps,
) -> bool {
    let needed_openers = balance_negate(suffix_balance);
    let prefix_balance = compute_projected_prefix_balance(
        group,
        file_lines,
        deleted_lines,
        inserted_by_line,
        inserted_line_maps,
    );
    let uncovered_prefix_balance = balance_sum(prefix_balance, covered_below_balance);
    balance_covers(uncovered_prefix_balance, needed_openers)
}

fn find_dropped_suffix_closers(
    group: &ReplacementGroup,
    file_lines: &[String],
    delta: DelimiterBalance,
    remaining_delta: DelimiterBalance,
    deleted_prefix_balance: DelimiterBalance,
    deleted_lines: &HashSet<u32>,
    inserted_by_line: &HashMap<u32, Vec<String>>,
    inserted_line_maps: &InsertedLineMaps,
) -> Option<DroppedSuffixClosers> {
    let mut suffix_length = 0u32;
    while (suffix_length as usize) < group.delete_indices.len() {
        let idx = group.end_line as usize - suffix_length as usize - 1;
        let text = file_lines.get(idx).map(|s| s.as_str()).unwrap_or("");
        if !STRUCTURAL_CLOSER_RE.is_match(text) {
            break;
        }
        suffix_length += 1;
    }
    if suffix_length == 0 {
        return None;
    }

    let suffix_start_line = group.end_line - suffix_length + 1;
    let suffix_lines: Vec<String> = file_lines
        [group.end_line as usize - suffix_length as usize..group.end_line as usize]
        .to_vec();
    let restated_head = count_payload_restated_suffix_head(&group.payload, &suffix_lines);
    let covered_tail = count_projected_below_suffix_tail(
        group,
        file_lines,
        deleted_lines,
        inserted_line_maps,
        &suffix_lines,
    );
    let keep_start = restated_head;
    let keep_end = suffix_length as usize - covered_tail;
    if keep_start >= keep_end {
        return None;
    }

    let kept_lines: Vec<String> = suffix_lines[keep_start..keep_end].to_vec();
    let kept_balance = compute_delimiter_balance(&kept_lines);
    let needed_openers = balance_negate(kept_balance);
    let covered_below_balance =
        compute_delimiter_balance(&suffix_lines[keep_end..]);
    if !balance_covers(delta, needed_openers) {
        return None;
    }
    if balance_covers(deleted_prefix_balance, needed_openers) {
        return None;
    }
    if !balance_covers(remaining_delta, needed_openers) {
        return None;
    }
    if !prefix_can_cover_suffix_closers(
        group,
        file_lines,
        kept_balance,
        covered_below_balance,
        deleted_lines,
        inserted_by_line,
        inserted_line_maps,
    ) {
        return None;
    }
    Some(DroppedSuffixClosers {
        start_line: suffix_start_line + keep_start as u32,
        count: (keep_end - keep_start) as u32,
        balance: kept_balance,
    })
}

// ── Boundary echo ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct BoundaryEcho {
    leading: usize,
    trailing: usize,
}

fn has_non_whitespace(text: &str) -> bool {
    text.chars().any(|c| !matches!(c, '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' '))
}

fn count_duplicate_leading_boundary_lines(
    group: &ReplacementGroup,
    file_lines: &[String],
) -> usize {
    let payload_len = group.payload.len();
    let max = payload_len.min(group.start_line as usize - 1);
    for count in (1..=max).rev() {
        let mut matches = true;
        let mut has_content = false;
        for offset in 0..count {
            let line = &group.payload[offset];
            if line != &file_lines[group.start_line as usize - 1 - count + offset] {
                matches = false;
                break;
            }
            has_content |= has_non_whitespace(line);
        }
        if matches && has_content {
            return count;
        }
    }
    0
}

fn count_duplicate_trailing_boundary_lines(
    group: &ReplacementGroup,
    file_lines: &[String],
) -> usize {
    let payload_len = group.payload.len();
    let max = payload_len.min(file_lines.len() - group.end_line as usize);
    for count in (1..=max).rev() {
        let mut matches = true;
        let mut has_content = false;
        for offset in 0..count {
            let line = &group.payload[payload_len - count + offset];
            if line != &file_lines[group.end_line as usize + offset] {
                matches = false;
                break;
            }
            has_content |= has_non_whitespace(line);
        }
        if matches && has_content {
            return count;
        }
    }
    0
}

fn find_boundary_echo(
    group: &ReplacementGroup,
    file_lines: &[String],
) -> Option<BoundaryEcho> {
    let leading_max = count_duplicate_leading_boundary_lines(group, file_lines);
    if leading_max == 0 {
        return None;
    }
    let trailing_max = count_duplicate_trailing_boundary_lines(group, file_lines);
    if trailing_max == 0 {
        return None;
    }
    if leading_max + trailing_max >= group.payload.len() {
        return None;
    }
    let leading_balance =
        compute_delimiter_balance(&group.payload[..leading_max]);
    let trailing_balance =
        compute_delimiter_balance(&group.payload[group.payload.len() - trailing_max..]);
    let dropped_balance = balance_delta(leading_balance, balance_negate(trailing_balance));
    if !balance_is_zero(dropped_balance) {
        let delta = balance_delta(
            compute_delimiter_balance(&group.payload),
            compute_delimiter_balance(
                &file_lines[group.start_line as usize - 1..group.end_line as usize],
            ),
        );
        if !balance_equal(dropped_balance, delta) {
            return None;
        }
    }
    Some(BoundaryEcho {
        leading: leading_max,
        trailing: trailing_max,
    })
}

fn describe_boundary_echo_repair(group: &ReplacementGroup, echo: BoundaryEcho) -> String {
    format!(
        "Auto-repaired a replacement boundary echo at line {}: \
         dropped {} leading and {} trailing payload line(s) already present outside the range. \
         Issue the payload as the final desired content for the selected range only — \
         never restate unchanged lines bordering the range.",
        group.start_line, echo.leading, echo.trailing
    )
}

fn describe_boundary_repair(group: &ReplacementGroup, action: &str) -> String {
    format!(
        "Auto-repaired a delimiter-balance mismatch in the replacement at line {}: {}. \
         Issue the payload as the final desired content only — \
         never restate or omit a closing bracket bordering the range.",
        group.start_line, action
    )
}

// ── One-sided boundary echo ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum EchoSide {
    Leading,
    Trailing,
}

#[derive(Debug, Clone, Copy)]
struct OneSidedEcho {
    side: EchoSide,
    count: usize,
}

fn find_one_sided_boundary_echo(
    group: &ReplacementGroup,
    file_lines: &[String],
) -> Option<OneSidedEcho> {
    let leading = count_duplicate_leading_boundary_lines(group, file_lines);
    let trailing = count_duplicate_trailing_boundary_lines(group, file_lines);
    if (leading > 0) == (trailing > 0) {
        return None;
    }
    let side = if leading > 0 {
        EchoSide::Leading
    } else {
        EchoSide::Trailing
    };
    let count = if leading > 0 { leading } else { trailing };
    if count >= group.payload.len() {
        return None;
    }
    let echo_lines: Vec<String> = match side {
        EchoSide::Leading => group.payload[..count].to_vec(),
        EchoSide::Trailing => group.payload[group.payload.len() - count..].to_vec(),
    };
    if !balance_is_zero(compute_delimiter_balance(&echo_lines)) {
        return None;
    }
    if group.delete_indices.len() <= 1 {
        match side {
            EchoSide::Trailing if echo_lines.iter().all(|l| is_structural_closer_line(l)) => {
                let payload_prefix: Vec<String> =
                    group.payload[..group.payload.len() - count].to_vec();
                if payload_has_jsx_opener_for_echo(&payload_prefix, &echo_lines) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(OneSidedEcho { side, count })
}

fn describe_one_sided_echo_repair(
    group: &ReplacementGroup,
    side: EchoSide,
    count: usize,
) -> String {
    let where_ = match side {
        EchoSide::Leading => "above",
        EchoSide::Trailing => "below",
    };
    let side_str = match side {
        EchoSide::Leading => "leading",
        EchoSide::Trailing => "trailing",
    };
    format!(
        "Auto-repaired a replacement boundary echo at line {}: \
         dropped {} {} payload line(s) identical to the surviving line(s) just {} the range. \
         The range was one line short of the content you retyped — \
         issue the payload as the final content for the selected range only, \
         and widen the range to consume any keeper you restate.",
        group.start_line, count, side_str, where_
    )
}

// ── Repair slot (pass 1 / pass 2) ─────────────────────────────────────────

#[derive(Debug, Clone)]
enum RepairSlot {
    Edits {
        edits: Vec<IndexedEdit>,
        warning: Option<String>,
    },
    Candidate {
        group: ReplacementGroup,
        inserts: Vec<IndexedEdit>,
        deletes: Vec<IndexedEdit>,
        delta: DelimiterBalance,
    },
}

fn net_deleted_prefix_balance(
    group: &ReplacementGroup,
    deleted_lines: &HashSet<u32>,
    inserted_by_line: &HashMap<u32, Vec<String>>,
    file_lines: &[String],
) -> DelimiterBalance {
    let mut deleted: Vec<String> = Vec::new();
    let mut inserted: Vec<String> = Vec::new();
    let mut line = group.start_line as i64 - 1;
    while line >= 1 && deleted_lines.contains(&(line as u32)) {
        deleted.insert(0, file_lines[line as usize - 1].clone());
        if let Some(inserted_at_line) = inserted_by_line.get(&(line as u32)) {
            inserted.splice(0..0, inserted_at_line.iter().cloned());
        }
        line -= 1;
    }
    balance_delta(
        compute_delimiter_balance(&deleted),
        compute_delimiter_balance(&inserted),
    )
}

fn slot_patch_delta(slot: &RepairSlot, file_lines: &[String]) -> DelimiterBalance {
    match slot {
        RepairSlot::Candidate { delta, .. } => *delta,
        RepairSlot::Edits { edits, .. } => {
            let mut inserted: Vec<String> = Vec::new();
            let mut deleted: Vec<String> = Vec::new();
            for entry in edits {
                match &entry.edit {
                    Edit::Insert { text, .. } => inserted.push(text.clone()),
                    Edit::Delete { anchor } => {
                        deleted.push(
                            file_lines
                                .get(anchor.line as usize - 1)
                                .cloned()
                                .unwrap_or_default(),
                        );
                    }
                    Edit::Block { .. } => {}
                }
            }
            balance_delta(
                compute_delimiter_balance(&inserted),
                compute_delimiter_balance(&deleted),
            )
        }
    }
}

/// Normalize replacement groups so common off-by-one boundaries do not
/// duplicate unchanged surrounding lines or wrongly drop/keep structural
/// closers. Returns the repaired edits plus one warning per repaired group.
fn repair_replacement_boundaries(
    edits: &[IndexedEdit],
    file_lines: &[String],
) -> (Vec<IndexedEdit>, Vec<String>) {
    // Pass 1: local repairs.
    let mut slots: Vec<RepairSlot> = Vec::new();
    let mut i = 0;
    while i < edits.len() {
        let group = match find_replacement_group(edits, i) {
            Some(g) => g,
            None => {
                slots.push(RepairSlot::Edits {
                    edits: vec![edits[i].clone()],
                    warning: None,
                });
                i += 1;
                continue;
            }
        };
        let inserts: Vec<IndexedEdit> =
            group.insert_indices.iter().map(|&idx| edits[idx].clone()).collect();
        let deletes: Vec<IndexedEdit> = group
            .delete_indices
            .iter()
            .map(|&idx| edits[idx].clone())
            .collect();
        i = group.delete_indices[group.delete_indices.len() - 1] + 1;

        let boundary_echo = find_boundary_echo(&group, file_lines);
        if let Some(echo) = boundary_echo {
            let kept_inserts: Vec<IndexedEdit> = inserts
                [echo.leading..inserts.len() - echo.trailing]
                .to_vec();
            slots.push(RepairSlot::Edits {
                edits: {
                    let mut v = kept_inserts;
                    v.extend(deletes);
                    v
                },
                warning: Some(describe_boundary_echo_repair(&group, echo)),
            });
            continue;
        }

        let delta = balance_delta(
            compute_delimiter_balance(&group.payload),
            compute_delimiter_balance(
                &file_lines[group.start_line as usize - 1..group.end_line as usize],
            ),
        );
        if balance_is_zero(delta) {
            let one_sided = find_one_sided_boundary_echo(&group, file_lines);
            if let Some(os) = one_sided {
                let trimmed: Vec<IndexedEdit> = match os.side {
                    EchoSide::Leading => inserts[os.count..].to_vec(),
                    EchoSide::Trailing => inserts[..inserts.len() - os.count].to_vec(),
                };
                slots.push(RepairSlot::Edits {
                    edits: {
                        let mut v = trimmed;
                        v.extend(deletes);
                        v
                    },
                    warning: Some(describe_one_sided_echo_repair(&group, os.side, os.count)),
                });
                continue;
            }
            let mut v = inserts.clone();
            v.extend(deletes);
            slots.push(RepairSlot::Edits {
                edits: v,
                warning: None,
            });
            continue;
        }

        let dup_suffix = find_duplicate_suffix(&group, file_lines, delta);
        if dup_suffix > 0 {
            let kept_inserts: Vec<IndexedEdit> = inserts[..inserts.len() - dup_suffix].to_vec();
            slots.push(RepairSlot::Edits {
                edits: {
                    let mut v = kept_inserts;
                    v.extend(deletes);
                    v
                },
                warning: Some(describe_boundary_repair(
                    &group,
                    &format!(
                        "dropped {} duplicated trailing payload line(s) already present below the range",
                        dup_suffix
                    ),
                )),
            });
            continue;
        }
        let dup_prefix = find_duplicate_prefix(&group, file_lines, delta);
        if dup_prefix > 0 {
            let kept_inserts: Vec<IndexedEdit> = inserts[dup_prefix..].to_vec();
            slots.push(RepairSlot::Edits {
                edits: {
                    let mut v = kept_inserts;
                    v.extend(deletes);
                    v
                },
                warning: Some(describe_boundary_repair(
                    &group,
                    &format!(
                        "dropped {} duplicated leading payload line(s) already present above the range",
                        dup_prefix
                    ),
                )),
            });
            continue;
        }
        slots.push(RepairSlot::Candidate {
            group: group.clone(),
            inserts,
            deletes,
            delta,
        });
    }

    // Project edits for pass 2.
    let mut projected: Vec<IndexedEdit> = Vec::new();
    for slot in &slots {
        match slot {
            RepairSlot::Candidate { inserts, deletes, .. } => {
                projected.extend(inserts.iter().cloned());
                projected.extend(deletes.iter().cloned());
            }
            RepairSlot::Edits { edits, .. } => projected.extend(edits.iter().cloned()),
        }
    }
    let mut deleted_lines: HashSet<u32> = HashSet::new();
    for entry in &projected {
        if let Edit::Delete { anchor } = &entry.edit {
            deleted_lines.insert(anchor.line);
        }
    }
    let mut inserted_by_line: HashMap<u32, Vec<String>> = HashMap::new();
    let mut inserted_line_maps = InsertedLineMaps::default();
    for entry in &projected {
        let Edit::Insert { cursor, text, .. } = &entry.edit else { continue };
        for anchor in get_cursor_anchors(cursor) {
            inserted_by_line.entry(anchor.line).or_default().push(text.clone());
        }
        match cursor {
            Cursor::BeforeAnchor(a) => {
                inserted_line_maps
                    .before
                    .entry(a.line)
                    .or_default()
                    .push(text.clone());
            }
            Cursor::AfterAnchor(a) => {
                inserted_line_maps
                    .after
                    .entry(a.line)
                    .or_default()
                    .push(text.clone());
            }
            Cursor::Bof | Cursor::Eof => {}
        }
    }
    let mut remaining_delta = DelimiterBalance::default();
    for slot in &slots {
        remaining_delta = balance_sum(remaining_delta, slot_patch_delta(slot, file_lines));
    }

    // Pass 2: missing-closer repair.
    let mut out: Vec<IndexedEdit> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for slot in &slots {
        match slot {
            RepairSlot::Edits { edits, warning } => {
                if let Some(w) = warning {
                    warnings.push(w.clone());
                }
                out.extend(edits.iter().cloned());
            }
            RepairSlot::Candidate {
                group,
                inserts,
                deletes,
                delta,
            } => {
                let deleted_prefix_balance = net_deleted_prefix_balance(
                    group,
                    &deleted_lines,
                    &inserted_by_line,
                    file_lines,
                );
                let dropped_closers = find_dropped_suffix_closers(
                    group,
                    file_lines,
                    *delta,
                    remaining_delta,
                    deleted_prefix_balance,
                    &deleted_lines,
                    &inserted_by_line,
                    &inserted_line_maps,
                );
                if let Some(dc) = dropped_closers {
                    warnings.push(describe_boundary_repair(
                        group,
                        &format!(
                            "kept {} structural closing line(s) the range deleted without restating",
                            dc.count
                        ),
                    ));
                    out.extend(inserts.iter().cloned());
                    let keep_start = dc.start_line;
                    let keep_end = dc.start_line + dc.count;
                    for entry in deletes {
                        if let Edit::Delete { anchor } = &entry.edit {
                            if anchor.line < keep_start || anchor.line >= keep_end {
                                out.push(entry.clone());
                            }
                        } else {
                            out.push(entry.clone());
                        }
                    }
                    let mut line = keep_start;
                    while line < keep_end {
                        deleted_lines.remove(&line);
                        line += 1;
                    }
                    remaining_delta = balance_sum(remaining_delta, dc.balance);
                    continue;
                }
                out.extend(inserts.iter().cloned());
                out.extend(deletes.iter().cloned());
            }
        }
    }
    (out, warnings)
}

// ── After-insert landing correction ───────────────────────────────────────

fn leading_indent(line: &str) -> String {
    let mut end = 0;
    for (i, c) in line.char_indices() {
        if c == '\t' || c == ' ' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    line[..end].to_string()
}

/// `deeper` strictly extends `shallower` (same indent style, more depth).
fn is_indent_deeper(deeper: &str, shallower: &str) -> bool {
    deeper.len() > shallower.len() && deeper.starts_with(shallower)
}

#[derive(Debug, Clone)]
struct AfterInsertGroup {
    anchor: u32,
    members: Vec<usize>,
    block_start: Option<u32>,
}

/// Depth of an after-insert hunk's body: the shallowest indentation across
/// its non-blank rows. Returns `None` when no depth claim can be made.
fn body_target_indent(rows: &[String]) -> Option<String> {
    let non_blank: Vec<&String> = rows.iter().filter(|r| has_non_whitespace(r)).collect();
    if non_blank.is_empty() {
        return None;
    }
    if non_blank.iter().all(|r| STRUCTURAL_CLOSER_RE.is_match(r)) {
        return None;
    }
    let mut target = leading_indent(non_blank[0]);
    for row in &non_blank {
        let indent = leading_indent(row);
        if indent.starts_with(&target) {
            continue;
        }
        if target.starts_with(&indent) {
            target = indent;
        } else {
            return None;
        }
    }
    Some(target)
}

/// Resolve where an after-insert hunk anchored on `group.anchor` should land
/// given its body depth `target`.
fn resolve_shifted_landing(
    group: &AfterInsertGroup,
    target: &str,
    file_lines: &[String],
    targeted_lines: &HashSet<u32>,
) -> Option<(u32, u32)> {
    let anchor_text = file_lines.get(group.anchor as usize - 1).map(|s| s.as_str())?;
    if !has_non_whitespace(anchor_text) {
        return None;
    }
    if !is_indent_deeper(&leading_indent(anchor_text), target) {
        return None;
    }
    let mut landing = group.anchor;
    let mut crossed = 0u32;
    let mut line = group.anchor + 1;
    while line as usize <= file_lines.len() {
        let text = file_lines.get(line as usize - 1).map(|s| s.as_str()).unwrap_or("");
        if !has_non_whitespace(text) {
            line += 1;
            continue;
        }
        if !STRUCTURAL_CLOSER_RE.is_match(text) {
            break;
        }
        let indent = leading_indent(text);
        if !indent.starts_with(target) {
            break;
        }
        if targeted_lines.contains(&line) {
            return None;
        }
        landing = line;
        crossed += 1;
        if indent.len() == target.len() {
            break;
        }
        line += 1;
    }
    if landing == group.anchor {
        None
    } else {
        Some((landing, crossed))
    }
}

/// Resolve where a block-lowered after-insert should land inward.
fn resolve_inward_landing(
    group: &AfterInsertGroup,
    target: &str,
    block_start: u32,
    file_lines: &[String],
    targeted_lines: &HashSet<u32>,
) -> Option<u32> {
    let anchor_text = file_lines.get(group.anchor as usize - 1).map(|s| s.as_str())?;
    if !has_non_whitespace(anchor_text) {
        return None;
    }
    if !STRUCTURAL_CLOSER_RE.is_match(anchor_text) {
        return None;
    }
    if !is_indent_deeper(target, &leading_indent(anchor_text)) {
        return None;
    }
    let mut landing = group.anchor;
    let mut line = group.anchor;
    while line > block_start {
        let text = file_lines.get(line as usize - 1).map(|s| s.as_str()).unwrap_or("");
        if !has_non_whitespace(text) {
            landing = line - 1;
            line -= 1;
            continue;
        }
        if !STRUCTURAL_CLOSER_RE.is_match(text) {
            break;
        }
        let indent = leading_indent(text);
        if !is_indent_deeper(target, &indent) {
            break;
        }
        if line != group.anchor && targeted_lines.contains(&line) {
            return None;
        }
        landing = line - 1;
        line -= 1;
    }
    if landing == group.anchor {
        None
    } else {
        Some(landing)
    }
}

fn after_insert_landing_shift_warning(
    anchor_line: u32,
    landing_line: u32,
    crossed: u32,
) -> String {
    let plural = if crossed == 1 { "" } else { "s" };
    format!(
        "INS.POST {}: body indented shallower than the anchor, so the landing moved past {} closing line{} to after line {}. \
         For the deeper position inside the block, re-issue with the body indented to match.",
        anchor_line, crossed, plural, landing_line
    )
}

fn block_insert_landing_shift_warning(
    block_start: u32,
    closer_line: u32,
    landing_line: u32,
) -> String {
    format!(
        "INS.BLK.POST {}: body indented deeper than closing line {}, so it was placed inside the block, after line {}. \
         `INS.BLK.POST` lands AFTER the block at sibling depth — if inside was intended, use plain `INS.POST {}:`.",
        block_start, closer_line, landing_line, closer_line
    )
}

/// Slide mis-anchored after-insert hunks to the depth their body indentation
/// claims. Returns the corrected edit list plus one warning per shifted hunk.
fn repair_after_insert_landings(
    edits: &[IndexedEdit],
    file_lines: &[String],
) -> (Vec<IndexedEdit>, Vec<String>) {
    // Group plain (non-replacement) after-anchor inserts per authored hunk.
    let mut groups: HashMap<(u32, u32), AfterInsertGroup> = HashMap::new();
    for (idx, entry) in edits.iter().enumerate() {
        let Edit::Insert { cursor, mode, .. } = &entry.edit else { continue };
        if *mode == InsertMode::Replacement {
            continue;
        }
        let Cursor::AfterAnchor(a) = cursor else { continue };
        let key = (a.line, entry.hunk_id);
        groups
            .entry(key)
            .and_modify(|g| g.members.push(idx))
            .or_insert_with(|| AfterInsertGroup {
                anchor: a.line,
                members: vec![idx],
                block_start: entry.block_start,
            });
    }
    if groups.is_empty() {
        return (edits.to_vec(), Vec::new());
    }

    // Lines explicitly targeted by any edit; a shift never crosses them.
    let mut targeted_lines: HashSet<u32> = HashSet::new();
    for entry in edits {
        match &entry.edit {
            Edit::Delete { anchor } => {
                targeted_lines.insert(anchor.line);
            }
            Edit::Insert { cursor, .. } => {
                if let Some(line) = cursor_anchor_line(cursor) {
                    targeted_lines.insert(line);
                }
            }
            Edit::Block { anchor, .. } => {
                targeted_lines.insert(anchor.line);
            }
        }
    }

    let mut out: Option<Vec<IndexedEdit>> = None;
    let mut warnings: Vec<String> = Vec::new();
    for group in groups.values() {
        let rows: Vec<String> = group
            .members
            .iter()
            .filter_map(|&idx| match &edits[idx].edit {
                Edit::Insert { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let target = match body_target_indent(&rows) {
            Some(t) => t,
            None => continue,
        };
        let outward = resolve_shifted_landing(group, &target, file_lines, &targeted_lines);
        if let Some((landing, crossed)) = outward {
            let out_vec = out.get_or_insert_with(|| edits.to_vec());
            for &idx in &group.members {
                if let Edit::Insert { cursor, .. } = &out_vec[idx].edit {
                    if let Cursor::AfterAnchor(_) = cursor {
                        out_vec[idx].edit = Edit::Insert {
                            cursor: Cursor::AfterAnchor(Anchor { line: landing }),
                            text: match &edits[idx].edit {
                                Edit::Insert { text, .. } => text.clone(),
                                _ => continue,
                            },
                            mode: InsertMode::Insert,
                        };
                    }
                }
            }
            warnings.push(after_insert_landing_shift_warning(
                group.anchor,
                landing,
                crossed,
            ));
            continue;
        }
        let Some(block_start) = group.block_start else { continue };
        let inward =
            resolve_inward_landing(group, &target, block_start, file_lines, &targeted_lines);
        if let Some(landing) = inward {
            let out_vec = out.get_or_insert_with(|| edits.to_vec());
            for &idx in &group.members {
                if let Edit::Insert { cursor, .. } = &out_vec[idx].edit {
                    if let Cursor::AfterAnchor(_) = cursor {
                        out_vec[idx].edit = Edit::Insert {
                            cursor: Cursor::AfterAnchor(Anchor { line: landing }),
                            text: match &edits[idx].edit {
                                Edit::Insert { text, .. } => text.clone(),
                                _ => continue,
                            },
                            mode: InsertMode::Insert,
                        };
                    }
                }
            }
            warnings.push(block_insert_landing_shift_warning(
                block_start,
                group.anchor,
                landing,
            ));
        }
    }
    (out.unwrap_or_else(|| edits.to_vec()), warnings)
}

// ── Phantom line + bounds helpers ─────────────────────────────────────────

fn trailing_phantom_line(file_lines: &[String]) -> u32 {
    if file_lines.len() > 1 && file_lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        file_lines.len() as u32
    } else {
        0
    }
}

fn drop_trailing_phantom_deletes(
    edits: Vec<IndexedEdit>,
    file_lines: &[String],
) -> Vec<IndexedEdit> {
    let phantom_line = trailing_phantom_line(file_lines);
    if phantom_line == 0 {
        return edits;
    }
    edits
        .into_iter()
        .filter(|entry| match &entry.edit {
            Edit::Delete { anchor } => anchor.line != phantom_line,
            _ => true,
        })
        .collect()
}

fn validate_line_bounds(edits: &[IndexedEdit], file_lines: &[String]) -> Result<(), String> {
    for entry in edits {
        for anchor in get_edit_anchors(&entry.edit) {
            if anchor.line < 1 || anchor.line as usize > file_lines.len() {
                return Err(format!(
                    "Line {} does not exist (file has {} lines)",
                    anchor.line,
                    file_lines.len()
                ));
            }
        }
    }
    Ok(())
}

// ── Bucket helpers ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BucketEntry {
    edit: Edit,
    idx: usize,
}

fn bucket_anchor_edits_by_line(edits: &[BucketEntry]) -> HashMap<u32, Vec<BucketEntry>> {
    let mut by_line: HashMap<u32, Vec<BucketEntry>> = HashMap::new();
    for entry in edits {
        let line = match &entry.edit {
            Edit::Delete { anchor } => anchor.line,
            Edit::Insert { cursor, .. } => cursor_anchor_line(cursor).unwrap_or(0),
            Edit::Block { anchor, .. } => anchor.line,
        };
        by_line.entry(line).or_default().push(entry.clone());
    }
    by_line
}

fn insert_at_start(file_lines: &mut Vec<String>, line_origins: &mut Vec<LineOrigin>, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    if file_lines.len() == 1 && file_lines[0].is_empty() {
        file_lines.splice(0..1, lines.iter().cloned());
        line_origins.splice(
            0..1,
            std::iter::repeat_n(LineOrigin::Insert, lines.len()),
        );
        return;
    }
    file_lines.splice(0..0, lines.iter().cloned());
    line_origins.splice(0..0, std::iter::repeat_n(LineOrigin::Insert, lines.len()));
}

fn insert_at_end(
    file_lines: &mut Vec<String>,
    line_origins: &mut Vec<LineOrigin>,
    lines: &[String],
) -> Option<u32> {
    if lines.is_empty() {
        return None;
    }
    if file_lines.len() == 1 && file_lines[0].is_empty() {
        file_lines.splice(0..1, lines.iter().cloned());
        line_origins.splice(
            0..1,
            std::iter::repeat_n(LineOrigin::Insert, lines.len()),
        );
        return Some(1);
    }
    let has_trailing_newline = file_lines.last().map(|s| s.is_empty()).unwrap_or(false);
    let insert_index = if has_trailing_newline {
        file_lines.len() - 1
    } else {
        file_lines.len()
    };
    file_lines.splice(insert_index..insert_index, lines.iter().cloned());
    line_origins.splice(
        insert_index..insert_index,
        std::iter::repeat_n(LineOrigin::Insert, lines.len()),
    );
    Some(insert_index as u32 + 1)
}

// ── Building indexed edits ────────────────────────────────────────────────
//
// We reconstruct `hunk_id` from structural adjacency: consecutive edits that
// belong to the same authored hunk share the same id. For replacement
// inserts (SWAP body), all body rows + their contiguous deletes form one
// hunk. For plain inserts, consecutive rows at the same anchor/cursor share
// a hunk. For deletes, each contiguous run of deletes at consecutive lines
// from the same source form a hunk.
//
// Because the parser emits edits in source order with all rows of one hunk
// contiguous, we can assign hunk ids by tracking transitions.

fn build_indexed_edits(edits: &[Edit], options: &ApplyOptions) -> Vec<IndexedEdit> {
    let mut result = Vec::with_capacity(edits.len());
    let mut current_hunk = 0u32;
    let mut prev_signature: Option<(u32, u8)> = None;
    // signature: (anchor_line, kind_code) where kind_code: 0=rep-insert-before,
    // 1=plain-insert, 2=delete, 3=block.

    for (idx, edit) in edits.iter().enumerate() {
        let (anchor_line, kind_code) = match edit {
            Edit::Insert {
                cursor,
                mode,
                ..
            } => {
                let al = cursor_anchor_line(cursor).unwrap_or(0);
                let kc = if *mode == InsertMode::Replacement { 0 } else { 1 };
                (al, kc)
            }
            Edit::Delete { anchor } => (anchor.line, 2),
            Edit::Block { anchor, .. } => (anchor.line, 3),
        };
        // A new hunk starts when the signature changes. For replacement
        // inserts the anchor stays the same across the body; the deletes that
        // follow share the same anchor (start of range), so they keep the same
        // hunk id. When a delete's anchor differs from the previous, it's a
        // new hunk — UNLESS it's a continuation of a replacement group's
        // contiguous deletes (anchor == prev_anchor + 1 and prev was a delete
        // from the same group). We handle that below.
        let new_hunk = match &prev_signature {
            None => true,
            Some((prev_anchor, prev_kind)) => {
                if *prev_kind == 0 && kind_code == 0 {
                    // continuation of replacement body
                    anchor_line != *prev_anchor
                } else if *prev_kind == 0 && kind_code == 2 {
                    // first delete after replacement inserts — same hunk
                    false
                } else if *prev_kind == 2 && kind_code == 2 {
                    // contiguous delete: same hunk if consecutive line
                    anchor_line != *prev_anchor + 1
                } else if *prev_kind == 1 && kind_code == 1 {
                    // contiguous plain insert at same anchor — same hunk
                    anchor_line != *prev_anchor
                } else {
                    true
                }
            }
        };
        if new_hunk && prev_signature.is_some() {
            current_hunk += 1;
        }
        let block_start = options.block_starts.get(&idx).copied();
        result.push(IndexedEdit {
            edit: edit.clone(),
            idx,
            block_start,
            hunk_id: current_hunk,
        });
        prev_signature = Some((anchor_line, kind_code));
    }
    result
}

// ── Main entry point ──────────────────────────────────────────────────────

const UNRESOLVED_BLOCK_INTERNAL: &str =
    "internal error: unresolved `SWAP.BLK` edit reached the applier (resolveBlockEdits was not run).";

/// Apply a parsed list of edits to a text body. Pure function — no I/O.
///
/// Returns the post-edit text and any diagnostic warnings collected by the
/// boundary-repair and landing-shift passes.
pub fn apply_edits(text: &str, edits: &[Edit]) -> ApplyResult {
    apply_edits_with_options(text, edits, &ApplyOptions::default())
}

/// Apply edits with caller-supplied block-resolution metadata.
pub fn apply_edits_with_options(
    text: &str,
    edits: &[Edit],
    options: &ApplyOptions,
) -> ApplyResult {
    if edits.is_empty() {
        return ApplyResult {
            text: text.to_string(),
            warnings: Vec::new(),
            first_changed_line: None,
            block_resolutions: Vec::new(),
        };
    }

    // Block edits must be resolved before apply.
    for edit in edits {
        if matches!(edit, Edit::Block { .. }) {
            return ApplyResult {
                text: text.to_string(),
                warnings: vec![UNRESOLVED_BLOCK_INTERNAL.to_string()],
                first_changed_line: None,
                block_resolutions: Vec::new(),
            };
        }
    }

    let mut file_lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    let mut line_origins: Vec<LineOrigin> = vec![LineOrigin::Original; file_lines.len()];

    let indexed = build_indexed_edits(edits, options);
    let target_edits = drop_trailing_phantom_deletes(indexed, &file_lines);
    if let Err(e) = validate_line_bounds(&target_edits, &file_lines) {
        return ApplyResult {
            text: text.to_string(),
            warnings: vec![e],
            first_changed_line: None,
            block_resolutions: Vec::new(),
        };
    }
    let (repaired, boundary_warnings) = repair_replacement_boundaries(&target_edits, &file_lines);
    let (landed, landing_warnings) = repair_after_insert_landings(&repaired, &file_lines);
    let mut warnings = boundary_warnings;
    warnings.extend(landing_warnings);

    // Partition edits into bof, eof, and anchor-targeted buckets.
    let mut bof_lines: Vec<String> = Vec::new();
    let mut eof_lines: Vec<String> = Vec::new();
    let mut anchor_edits: Vec<BucketEntry> = Vec::new();
    for (idx, entry) in landed.iter().enumerate() {
        match &entry.edit {
            Edit::Insert {
                cursor: Cursor::Bof,
                text,
                ..
            } => {
                bof_lines.push(text.clone());
            }
            Edit::Insert {
                cursor: Cursor::Eof,
                text,
                ..
            } => {
                eof_lines.push(text.clone());
            }
            _ => {
                anchor_edits.push(BucketEntry {
                    edit: entry.edit.clone(),
                    idx,
                });
            }
        }
    }

    // Apply per-line buckets bottom-up so earlier indices stay valid.
    let by_line = bucket_anchor_edits_by_line(&anchor_edits);
    let mut lines_sorted: Vec<u32> = by_line.keys().copied().collect();
    lines_sorted.sort_by(|a, b| b.cmp(a));
    for line in lines_sorted {
        let bucket = match by_line.get(&line) {
            Some(b) => b,
            None => continue,
        };
        let mut bucket = bucket.clone();
        bucket.sort_by(|a, b| a.idx.cmp(&b.idx));

        let idx = line as usize - 1;
        let current_line = file_lines.get(idx).cloned().unwrap_or_default();
        let mut before_insert_lines: Vec<String> = Vec::new();
        let mut after_insert_lines: Vec<String> = Vec::new();
        let mut replacement_lines: Vec<String> = Vec::new();
        let mut delete_line = false;

        for entry in &bucket {
            match &entry.edit {
                Edit::Insert {
                    cursor,
                    text,
                    mode,
                } if *mode == InsertMode::Replacement => {
                    replacement_lines.push(text.clone());
                }
                Edit::Insert {
                    cursor: Cursor::AfterAnchor(_),
                    text,
                    ..
                } => {
                    after_insert_lines.push(text.clone());
                }
                Edit::Insert { text, .. } => {
                    before_insert_lines.push(text.clone());
                }
                Edit::Delete { .. } => {
                    delete_line = true;
                }
                Edit::Block { .. } => {}
            }
        }
        if before_insert_lines.is_empty()
            && replacement_lines.is_empty()
            && after_insert_lines.is_empty()
            && !delete_line
        {
            continue;
        }

        let before_count = before_insert_lines.len();
        let replacement_count = replacement_lines.len();
        let after_count = after_insert_lines.len();

        let replacement: Vec<String> = if delete_line {
            let mut v = before_insert_lines.clone();
            v.extend(replacement_lines);
            v.extend(after_insert_lines);
            v
        } else {
            let mut v = before_insert_lines.clone();
            v.extend(replacement_lines);
            v.push(current_line);
            v.extend(after_insert_lines);
            v
        };
        let mut origins: Vec<LineOrigin> = Vec::new();
        for _ in 0..before_count {
            origins.push(LineOrigin::Insert);
        }
        for _ in 0..replacement_count {
            origins.push(if delete_line {
                LineOrigin::Replacement
            } else {
                LineOrigin::Insert
            });
        }
        if !delete_line {
            origins.push(line_origins.get(idx).copied().unwrap_or(LineOrigin::Original));
        }
        for _ in 0..after_count {
            origins.push(LineOrigin::Insert);
        }

        file_lines.splice(idx..idx + 1, replacement.iter().cloned());
        line_origins.splice(idx..idx + 1, origins);
    }

    if !bof_lines.is_empty() {
        insert_at_start(&mut file_lines, &mut line_origins, &bof_lines);
    }
    insert_at_end(&mut file_lines, &mut line_origins, &eof_lines);

    ApplyResult {
        text: file_lines.join("\n"),
        warnings,
        first_changed_line: None,
        block_resolutions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_before(line: u32, text: &str) -> Edit {
        Edit::Insert {
            cursor: Cursor::BeforeAnchor(Anchor { line }),
            text: text.to_string(),
            mode: InsertMode::Replacement,
        }
    }

    fn delete(line: u32) -> Edit {
        Edit::Delete {
            anchor: Anchor { line },
        }
    }

    fn insert_after_plain(line: u32, text: &str) -> Edit {
        Edit::Insert {
            cursor: Cursor::AfterAnchor(Anchor { line }),
            text: text.to_string(),
            mode: InsertMode::Insert,
        }
    }

    fn insert_head(text: &str) -> Edit {
        Edit::Insert {
            cursor: Cursor::Bof,
            text: text.to_string(),
            mode: InsertMode::Insert,
        }
    }

    fn insert_tail(text: &str) -> Edit {
        Edit::Insert {
            cursor: Cursor::Eof,
            text: text.to_string(),
            mode: InsertMode::Insert,
        }
    }

    // SWAP 2.=2: +BBB on "aaa\nbbb\nccc"
    fn swap_single(line: u32, payload: &[&str]) -> Vec<Edit> {
        let mut edits = Vec::new();
        for p in payload {
            edits.push(insert_before(line, p));
        }
        edits.push(delete(line));
        edits
    }

    fn swap_range(start: u32, end: u32, payload: &[&str]) -> Vec<Edit> {
        let mut edits = Vec::new();
        for p in payload {
            edits.push(insert_before(start, p));
        }
        for l in start..=end {
            edits.push(delete(l));
        }
        edits
    }

    #[test]
    fn ins_post_basic() {
        // applyDiff("aaa\nbbb\nccc", "INS.POST 2:\n+after b")
        let edits = vec![insert_after_plain(2, "after b")];
        let result = apply_edits("aaa\nbbb\nccc", &edits);
        assert_eq!(result.text, "aaa\nbbb\nafter b\nccc");
    }

    #[test]
    fn del_single() {
        let edits = vec![delete(2)];
        let result = apply_edits("aaa\nbbb\nccc", &edits);
        assert_eq!(result.text, "aaa\nccc");
    }

    #[test]
    fn swap_single_replaces() {
        let edits = swap_single(2, &["BBB"]);
        let result = apply_edits("aaa\nbbb\nccc", &edits);
        assert_eq!(result.text, "aaa\nBBB\nccc");
    }

    #[test]
    fn ins_head() {
        let edits = vec![insert_head("top")];
        let result = apply_edits("aaa\nbbb\nccc", &edits);
        assert_eq!(result.text, "top\naaa\nbbb\nccc");
    }

    #[test]
    fn ins_tail() {
        let edits = vec![insert_tail("tail")];
        let result = apply_edits("aaa\nbbb\nccc", &edits);
        assert_eq!(result.text, "aaa\nbbb\nccc\ntail");
    }

    #[test]
    fn empty_swap_is_delete() {
        // SWAP 2.=2: with no body → delete line 2
        let edits = vec![delete(2)];
        let result = apply_edits("aaa\nbbb\nccc", &edits);
        assert_eq!(result.text, "aaa\nccc");
    }

    #[test]
    fn ins_tail_keeps_echoes_literal() {
        // INS.TAIL: +bbb +ccc +NEW on "aaa\nbbb\nccc"
        let edits = vec![
            insert_tail("bbb"),
            insert_tail("ccc"),
            insert_tail("NEW"),
        ];
        let result = apply_edits("aaa\nbbb\nccc", &edits);
        assert_eq!(result.text, "aaa\nbbb\nccc\nbbb\nccc\nNEW");
    }

    #[test]
    fn del_range() {
        // DEL 2.=3
        let edits = vec![delete(2), delete(3)];
        let result = apply_edits("line1\nline2\nline3\nline4", &edits);
        assert_eq!(result.text, "line1\nline4");
    }

    #[test]
    fn swap_range_replaces() {
        // SWAP 2.=3: +X +Y
        let edits = swap_range(2, 3, &["X", "Y"]);
        let result = apply_edits("aaa\nbbb\nccc\nddd", &edits);
        assert_eq!(result.text, "aaa\nX\nY\nddd");
    }

    #[test]
    fn preserves_blank_replacement_rows() {
        // SWAP 2.=2: with two empty payload rows
        let edits = swap_single(2, &["", ""]);
        let result = apply_edits("a\nb\nc\nd\ne", &edits);
        assert_eq!(result.text, "a\n\n\nc\nd\ne");
    }

    // ── Boundary repair tests ──────────────────────────────────────────────

    #[test]
    fn drops_duplicated_structural_closer() {
        let file = "it('a', () => {\n\tsetup();\n\trun();\n});\nafter();";
        // SWAP 2.=3: +\tsetup2(); +\trun2(); +});
        let edits = swap_range(2, 3, &["\tsetup2();", "\trun2();", "});"]);
        let result = apply_edits(file, &edits);
        assert_eq!(
            result.text,
            "it('a', () => {\n\tsetup2();\n\trun2();\n});\nafter();"
        );
        assert!(result.warnings.iter().any(|w| w.contains("delimiter-balance")));
    }

    #[test]
    fn drops_boundary_echo_leading_trailing() {
        let file = "func _cmd_travel_homeworld():\n\tvar destination = get_homeworld()\n\ttravel_to(destination)\n\tprint_status()";
        let edits = swap_range(2, 3, &[
            "func _cmd_travel_homeworld():",
            "\tvar destination = find_homeworld()",
            "\ttravel_to(destination)",
            "\tprint_status()",
        ]);
        let result = apply_edits(file, &edits);
        assert_eq!(
            result.text,
            "func _cmd_travel_homeworld():\n\tvar destination = find_homeworld()\n\ttravel_to(destination)\n\tprint_status()"
        );
        assert!(result.warnings.iter().any(|w| w.contains("boundary echo")));
    }

    #[test]
    fn preserves_balance_neutral_duplicated_statement() {
        let file = "a = 1;\nb = 2;\nc = 3;";
        let edits = swap_single(1, &["a = 1;", "b = 2;"]);
        let result = apply_edits(file, &edits);
        assert_eq!(result.text, "a = 1;\nb = 2;\nb = 2;\nc = 3;");
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn spares_deleted_closing_line_when_payload_omits_it() {
        let file = "const handlers = {\n\ta() {\n\t\treturn 1;\n\t},\n};";
        // SWAP 5.=5: +\tb() { +\t\treturn 2; +\t},
        let edits = swap_single(5, &["\tb() {", "\t\treturn 2;", "\t},"]);
        let result = apply_edits(file, &edits);
        assert_eq!(
            result.text,
            "const handlers = {\n\ta() {\n\t\treturn 1;\n\t},\n\tb() {\n\t\treturn 2;\n\t},\n};"
        );
        assert!(result.warnings.iter().any(|w| w.contains("delimiter-balance")));
    }

    // ── Landing shift tests ────────────────────────────────────────────────

    #[test]
    fn slides_shallower_body_past_closing_line() {
        let file = "function f() {\n    if (x) {\n        a();\n    }\n    b();\n}\n";
        let edits = vec![insert_after_plain(3, "    c();")];
        let result = apply_edits(file, &edits);
        assert_eq!(
            result.text,
            "function f() {\n    if (x) {\n        a();\n    }\n    c();\n    b();\n}\n"
        );
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("INS.POST 3:"));
        assert!(result.warnings[0].contains("moved past 1 closing line to after line 4"));
    }

    #[test]
    fn no_shift_when_body_matches_anchor_depth() {
        let file = "function f() {\n    if (x) {\n        a();\n    }\n    b();\n}\n";
        let edits = vec![insert_after_plain(3, "        c();")];
        let result = apply_edits(file, &edits);
        let lines: Vec<&str> = result.text.split('\n').collect();
        assert_eq!(lines[3], "        c();");
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn never_crosses_content_lines() {
        let py = "def f():\n    if x:\n        a()\n    b()\n";
        let edits = vec![insert_after_plain(3, "    c()")];
        let result = apply_edits(py, &edits);
        assert_eq!(result.text, "def f():\n    if x:\n        a()\n    c()\n    b()\n");
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn refuses_to_cross_targeted_line() {
        let file = "function f() {\n    if (x) {\n        a();\n    }\n    b();\n}\n";
        let edits = vec![insert_after_plain(3, "    c();"), delete(4)];
        let result = apply_edits(file, &edits);
        assert_eq!(
            result.text,
            "function f() {\n    if (x) {\n        a();\n    c();\n    b();\n}\n"
        );
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn leaves_ins_pre_untouched() {
        let file = "function f() {\n    if (x) {\n        a();\n    }\n    b();\n}\n";
        let edits = vec![Edit::Insert {
            cursor: Cursor::BeforeAnchor(Anchor { line: 4 }),
            text: "    c();".to_string(),
            mode: InsertMode::Insert,
        }];
        let result = apply_edits(file, &edits);
        let lines: Vec<&str> = result.text.split('\n').collect();
        assert_eq!(lines[3], "    c();");
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn ignores_brackets_inside_strings() {
        let file = "const a = \"}\";\nconst b = \"x\";\nconst c = \"y\";";
        let edits = swap_single(2, &["const b = \"}}}\";"]);
        let result = apply_edits(file, &edits);
        assert_eq!(result.text, "const a = \"}\";\nconst b = \"}}}\";\nconst c = \"y\";");
        assert!(result.warnings.is_empty());
    }
}
