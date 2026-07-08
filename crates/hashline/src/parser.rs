//! Parser — port of packages/hashline/src/parser.ts + input.ts
//!
//! Token-driven state machine that turns a stream of `Token`s (from the
//! tokenizer) into a flat list of `Edit`s, plus the section splitter that
//! carves a full `[path#TAG]`-prefixed input into `PatchSection`s.
//!
//! Two public entry points:
//! - `parse_patch(diff)` — parse a diff body (no section header) into edits.
//! - `parse_sections(input)` — parse full input with `[path#TAG]` headers.

use std::sync::LazyLock;

use regex::Regex;

use crate::format::{
    HL_FILE_HASH_LENGTH, HL_FILE_HASH_SEP, HL_FILE_PREFIX, HL_FILE_SUFFIX, HL_PAYLOAD_REPLACE,
    HL_RANGE_SEP,
};
use crate::messages::{
    BARE_BODY_AUTO_PIPED_WARNING, DELETE_BLOCK_TAKES_NO_BODY, EMPTY_INSERT, MINUS_ROW_REJECTED,
    MOVE_TAKES_NO_BODY, REM_TAKES_NO_BODY, delete_takes_no_body_message,
};
use crate::prefixes::strip_one_leading_hashline_prefix;
use crate::tokenizer::{
    clone_cursor, is_envelope_marker, is_op, split_hashline_lines, tokenize, BlockTarget, Token,
};
use crate::types::{
    Anchor, BlockMode, Cursor, Edit, FileOp, InsertMode, PatchSection, ParsedRange, SplitOptions,
};

// ── Public result type ───────────────────────────────────────────────────

/// Result of parsing a diff body into edits.
pub struct ParseResult {
    pub edits: Vec<Edit>,
    pub warnings: Vec<String>,
    pub file_op: Option<FileOp>,
}

// ── Range helpers ────────────────────────────────────────────────────────

fn validate_range_order(range: &ParsedRange, line_num: usize) -> Result<(), String> {
    if range.end < range.start {
        return Err(format!(
            "line {line_num}: range {}{}{} ends before it starts.",
            range.start, HL_RANGE_SEP, range.end
        ));
    }
    Ok(())
}

fn expand_range(range: &ParsedRange) -> Vec<Anchor> {
    (range.start..=range.end).map(|line| Anchor { line }).collect()
}

fn is_skippable_comment_line(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// Stripped remainder of a bare `N: <value>` row that is a lone quoted or
/// numeric literal (optionally comma-terminated) — the shape of a
/// numeric-keyed dict/YAML body rather than read-output paste.
static BARE_LITERAL_VALUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:"[^"]*"|'[^']*'|[-+]?\d+(?:\.\d+)?)\s*,?\s*$"#).unwrap()
});

// ── apply_patch / unified-diff contamination detection ───────────────────

fn detect_apply_patch_contamination(text: &str, _has_pending: bool) -> Option<String> {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let starts_with_sentinel = trimmed.starts_with("*** Update File:")
        || trimmed.starts_with("*** Add File:")
        || trimmed.starts_with("*** Delete File:")
        || trimmed.starts_with("*** Move to:");
    if starts_with_sentinel {
        let preview = if trimmed.len() > 48 {
            format!("{}…", &trimmed[..48])
        } else {
            trimmed.to_string()
        };
        return Some(format!(
            "apply_patch sentinel {preview:?} is not valid in hashline. \
             File sections start with `[path#HASH]` (no `Update File:` / `Add File:` keyword). \
             Use `SWAP N{HL_RANGE_SEP}M:`, `DEL N{HL_RANGE_SEP}M`, or `INS.PRE|POST|HEAD|TAIL:` ops."
        ));
    }
    let unified_re =
        Regex::new(r"^@@\s+[-+]?\d+,\d+\s+[-+]?\d+,\d+\s+@@").unwrap();
    if unified_re.is_match(trimmed) {
        return Some(format!(
            "unified-diff hunk header (`@@ -N,M +N,M @@`) is not valid in hashline. \
             Use `SWAP N{HL_RANGE_SEP}M:`, `DEL N{HL_RANGE_SEP}M`, or `INS.PRE|POST|HEAD|TAIL:` ops."
        ));
    }
    if trimmed.starts_with("@@") {
        let preview = if trimmed.len() > 48 {
            format!("{}…", &trimmed[..48])
        } else {
            trimmed.to_string()
        };
        return Some(format!(
            "`@@`-bracketed hunk header {preview:?} is not valid in hashline. \
             Drop the `@@ ... @@` brackets and write a verb header such as `SWAP N{HL_RANGE_SEP}M:`."
        ));
    }
    let del_with_colon_re =
        Regex::new(r"^DEL\s+[1-9]\d*(?:\s*(?:\.\.|\.=|-|…|\s)\s*[1-9]\d*)?\s*:").unwrap();
    if del_with_colon_re.is_match(trimmed) {
        return Some(format!(
            "`DEL N{HL_RANGE_SEP}M` has no colon and no body. Remove the colon and body rows."
        ));
    }
    let bare_number_re = Regex::new(r"^[1-9]\d*\s*$").unwrap();
    if bare_number_re.is_match(trimmed) {
        return Some(format!(
            "hunk headers need a verb. Use `SWAP {trimmed}{HL_RANGE_SEP}{trimmed}:` to replace, \
             or `DEL {trimmed}` to delete."
        ));
    }
    let bare_range_re = Regex::new(r"^([1-9]\d*)\s*[-. …=]+\s*([1-9]\d*)\s*:?$").unwrap();
    if let Some(caps) = bare_range_re.captures(trimmed) {
        let a = &caps[1];
        let b = &caps[2];
        return Some(format!(
            "bare range hunk header {trimmed:?} is not valid. \
             Hunk headers need a verb: write `SWAP {a}{HL_RANGE_SEP}{b}:` or `DEL {a}{HL_RANGE_SEP}{b}`."
        ));
    }
    None
}

// ── Pending payload rows ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PayloadRow {
    text: String,
    #[allow(dead_code)]
    line_num: usize,
    bare: bool,
}

#[derive(Debug, Clone)]
struct Pending {
    target: BlockTarget,
    line_num: usize,
    payloads: Vec<PayloadRow>,
    /// Blank rows seen after the body started. Interior blanks are committed
    /// to the payload when the next non-blank row arrives; trailing blanks
    /// before the next header/op are layout separators and are discarded on
    /// flush.
    deferred_blanks: Vec<PayloadRow>,
}

// ── Executor — token-driven state machine ────────────────────────────────

struct Executor {
    edits: Vec<Edit>,
    warnings: Vec<String>,
    edit_index: usize,
    pending: Option<Pending>,
    file_op: Option<FileOp>,
    terminated: bool,
    skippable_comments: Vec<PayloadRow>,
}

impl Executor {
    fn new() -> Self {
        Self {
            edits: Vec::new(),
            warnings: Vec::new(),
            edit_index: 0,
            pending: None,
            file_op: None,
            terminated: false,
            skippable_comments: Vec::new(),
        }
    }

    fn discard_pending_skippable_comments(&mut self) {
        self.skippable_comments.clear();
    }

    fn consume_pending_skippable_comments(&mut self) {
        if self.skippable_comments.is_empty() {
            return;
        }
        let comments = std::mem::take(&mut self.skippable_comments);
        for comment in comments {
            self.handle_raw(&comment.text, comment.line_num);
        }
    }

    fn feed(&mut self, token: &Token) {
        if self.terminated {
            return;
        }
        match token {
            Token::EnvelopeBegin { .. } => {
                self.consume_pending_skippable_comments();
            }
            Token::EnvelopeEnd { .. } => {
                self.consume_pending_skippable_comments();
                self.terminated = true;
            }
            Token::Abort { .. } => {
                self.terminated = true;
            }
            Token::SectionHeader { .. } => {
                self.consume_pending_skippable_comments();
                self.flush_pending();
            }
            Token::Blank { line_num } => {
                self.consume_pending_skippable_comments();
                self.handle_blank("", *line_num);
            }
            Token::BodyRow { text, line_num } => {
                self.consume_pending_skippable_comments();
                self.handle_literal_payload(text, *line_num);
            }
            Token::Raw { text, line_num } => {
                if self.pending.is_none() && is_skippable_comment_line(text) {
                    self.skippable_comments.push(PayloadRow {
                        text: text.clone(),
                        line_num: *line_num,
                        bare: true,
                    });
                    return;
                }
                self.consume_pending_skippable_comments();
                self.handle_raw(text, *line_num);
            }
            Token::HunkHeader { target, line_num } => {
                self.discard_pending_skippable_comments();
                self.handle_hunk_header(target, *line_num);
            }
        }
    }

    fn handle_hunk_header(&mut self, target: &BlockTarget, line_num: usize) {
        match target {
            BlockTarget::Replace { range } | BlockTarget::Delete { range } => {
                if let Err(e) = validate_range_order(range, line_num) {
                    self.warnings.push(e);
                }
            }
            _ => {}
        }
        match target {
            BlockTarget::Rem => {
                self.flush_pending();
                self.set_file_op(FileOp::Rem, line_num);
            }
            BlockTarget::Move { dest } => {
                self.flush_pending();
                self.set_file_op(FileOp::Mv { dest: dest.clone() }, line_num);
            }
            _ => {
                self.flush_pending();
                self.pending = Some(Pending {
                    target: target.clone(),
                    line_num,
                    payloads: Vec::new(),
                    deferred_blanks: Vec::new(),
                });
            }
        }
    }

    fn end(&mut self) -> ParseResult {
        self.consume_pending_skippable_comments();
        self.flush_pending();
        self.validate_file_op();
        self.validate_no_overlapping_deletes();
        ParseResult {
            edits: std::mem::take(&mut self.edits),
            warnings: std::mem::take(&mut self.warnings),
            file_op: self.file_op.take(),
        }
    }

    fn set_file_op(&mut self, file_op: FileOp, line_num: usize) {
        if self.file_op.is_some() {
            self.warnings.push(format!(
                "line {line_num}: only one file-level op (`REM` or `MV`) per section. \
                 Merge them under one header."
            ));
            return;
        }
        if matches!(file_op, FileOp::Rem) && !self.edits.is_empty() {
            self.warnings.push(format!("line {line_num}: {REM_TAKES_NO_BODY}"));
            return;
        }
        self.file_op = Some(file_op);
    }

    fn validate_file_op(&self) {
        // REM + edits is already rejected at set_file_op time; nothing more to do.
    }

    fn validate_no_overlapping_deletes(&mut self) {
        use std::collections::HashMap;
        let mut source_lines_by_anchor: HashMap<u32, Vec<usize>> = HashMap::new();
        for (idx, edit) in self.edits.iter().enumerate() {
            let anchor_line = match edit {
                Edit::Delete { anchor } => anchor.line,
                _ => continue,
            };
            let source_lines = source_lines_by_anchor.entry(anchor_line).or_default();
            // The line_num of the hunk is not stored on Edit in this Rust port;
            // use the edit index as a stable stand-in for ordering diagnostics.
            let _ = idx;
            source_lines.push(idx);
        }
        for (_anchor_line, source_lines) in &source_lines_by_anchor {
            if source_lines.len() < 2 {
                continue;
            }
            let mut sorted = source_lines.clone();
            sorted.sort_unstable();
            let first_block = sorted[0];
            let second_block = sorted[1];
            self.warnings.push(format!(
                "line {second_block}: anchor line is already targeted by another hunk on line {first_block}. \
                 Issue ONE hunk per range; payload is only the final desired content, never a before/after pair."
            ));
        }
    }

    fn handle_literal_payload(&mut self, text: &str, line_num: usize) {
        let pending = match &mut self.pending {
            Some(p) => p,
            None => {
                if self.file_op.is_some() {
                    self.warnings
                        .push(format!("line {line_num}: {MOVE_TAKES_NO_BODY}"));
                } else {
                    self.warnings.push(format!(
                        "line {line_num}: payload line has no preceding hunk header. \
                         Got {:?}.",
                        format!("{HL_PAYLOAD_REPLACE}{text}")
                    ));
                }
                return;
            }
        };
        let is_delete = matches!(pending.target, BlockTarget::Delete { .. });
        let is_delete_block = matches!(pending.target, BlockTarget::DeleteBlock { .. });
        if is_delete {
            self.warnings
                .push(format!("line {line_num}: {}", delete_takes_no_body_message()));
            return;
        }
        if is_delete_block {
            self.warnings
                .push(format!("line {line_num}: {DELETE_BLOCK_TAKES_NO_BODY}"));
            return;
        }
        let mut deferred = std::mem::take(&mut pending.deferred_blanks);
        if !deferred.is_empty() {
            if !self.warnings.iter().any(|w| w == BARE_BODY_AUTO_PIPED_WARNING) {
                self.warnings.push(BARE_BODY_AUTO_PIPED_WARNING.to_string());
            }
            pending.payloads.append(&mut deferred);
        }
        pending.payloads.push(PayloadRow {
            text: text.to_string(),
            line_num,
            bare: false,
        });
    }

    fn handle_raw(&mut self, text: &str, line_num: usize) {
        if let Some(contamination) = detect_apply_patch_contamination(text, self.pending.is_some())
        {
            self.warnings.push(format!("line {line_num}: {contamination}"));
            return;
        }
        if self.file_op.is_some() {
            self.warnings
                .push(format!("line {line_num}: {MOVE_TAKES_NO_BODY}"));
            return;
        }
        let has_pending = self.pending.is_some();
        if has_pending {
            if text.trim().is_empty() {
                self.handle_blank(text, line_num);
                return;
            }
            let pending = self.pending.as_mut().unwrap();
            let is_delete = matches!(pending.target, BlockTarget::Delete { .. });
            let is_delete_block = matches!(pending.target, BlockTarget::DeleteBlock { .. });
            if is_delete {
                self.warnings
                    .push(format!("line {line_num}: {}", delete_takes_no_body_message()));
                return;
            }
            if is_delete_block {
                self.warnings
                    .push(format!("line {line_num}: {DELETE_BLOCK_TAKES_NO_BODY}"));
                return;
            }
            if text.trim_start().as_bytes().first() == Some(&b'-') {
                self.warnings
                    .push(format!("line {line_num}: {MINUS_ROW_REJECTED}"));
                return;
            }
            if !self.warnings.iter().any(|w| w == BARE_BODY_AUTO_PIPED_WARNING) {
                self.warnings.push(BARE_BODY_AUTO_PIPED_WARNING.to_string());
            }
            let mut deferred = std::mem::take(&mut pending.deferred_blanks);
            if !deferred.is_empty() {
                pending.payloads.append(&mut deferred);
            }
            pending.payloads.push(PayloadRow {
                text: text.to_string(),
                line_num,
                bare: true,
            });
            return;
        }
        if text.trim().is_empty() {
            return;
        }
        self.warnings.push(format!(
            "line {line_num}: payload line has no preceding hunk header. \
             Use `SWAP N{HL_RANGE_SEP}M:`, `DEL N{HL_RANGE_SEP}M`, or `INS.PRE|POST|HEAD|TAIL:` above the body. Got {text:?}."
        ));
    }

    fn handle_blank(&mut self, text: &str, _line_num: usize) {
        let pending = match &self.pending {
            Some(p) => p,
            None => return,
        };
        let is_delete = matches!(pending.target, BlockTarget::Delete { .. })
            || matches!(pending.target, BlockTarget::DeleteBlock { .. });
        if is_delete {
            return;
        }
        if pending.payloads.is_empty() {
            return;
        }
        let pending = self.pending.as_mut().unwrap();
        pending.deferred_blanks.push(PayloadRow {
            text: text.to_string(),
            line_num: _line_num,
            bare: true,
        });
    }


    /// Strip a single read-output line-number prefix (`N:`) from every bare
    /// body row, but only when *all* bare rows carry one.
    fn strip_bare_prefixes_if_uniform(payloads: &mut [PayloadRow]) {
        let mut saw_bare = false;
        let mut all_literal_values = true;
        let mut stripped: Vec<Option<String>> = vec![None; payloads.len()];
        for (i, row) in payloads.iter().enumerate() {
            if !row.bare || row.text.trim().is_empty() {
                continue;
            }
            saw_bare = true;
            let s = strip_one_leading_hashline_prefix(&row.text);
            if s == row.text {
                return;
            }
            if !BARE_LITERAL_VALUE_RE.is_match(&s) {
                all_literal_values = false;
            }
            stripped[i] = Some(s);
        }
        if !saw_bare {
            return;
        }
        if all_literal_values {
            return;
        }
        for (i, row) in payloads.iter_mut().enumerate() {
            if row.bare && !row.text.trim().is_empty() {
                if let Some(s) = stripped[i].take() {
                    row.text = s;
                }
            }
        }
    }

    fn push_insert(&mut self, cursor: Cursor, text: String, mode: InsertMode) {
        self.edits.push(Edit::Insert {
            cursor: clone_cursor(&cursor),
            text,
            mode,
        });
        self.edit_index += 1;
    }

    fn push_delete(&mut self, anchor: Anchor) {
        self.edits.push(Edit::Delete { anchor });
        self.edit_index += 1;
    }

    fn push_block(&mut self, anchor: Anchor, payloads: &[PayloadRow], mode: BlockMode) {
        self.edits.push(Edit::Block {
            anchor,
            payloads: payloads.iter().map(|p| p.text.clone()).collect(),
            mode,
        });
        self.edit_index += 1;
    }

    fn emit_payload_rows(&mut self, cursor: Cursor, payloads: &[PayloadRow], mode: InsertMode) {
        for payload in payloads {
            self.push_insert(clone_cursor(&cursor), payload.text.clone(), mode);
        }
    }

    fn flush_pending(&mut self) {
        let pending = match self.pending.take() {
            Some(p) => p,
            None => return,
        };
        let Pending {
            target,
            line_num,
            mut payloads,
            ..
        } = pending;
        Self::strip_bare_prefixes_if_uniform(&mut payloads);
        match target {
            BlockTarget::Delete { range } => {
                for anchor in expand_range(&range) {
                    self.push_delete(anchor);
                }
            }
            BlockTarget::DeleteBlock { anchor } => {
                self.push_block(anchor, &[], BlockMode::Delete);
            }
            BlockTarget::Block { anchor } => {
                if payloads.is_empty() {
                    self.warnings.push(format!(
                        "line {line_num}: `SWAP.BLK N:` needs at least one `+TEXT` body row. \
                         To delete a block, use `DEL.BLK N`."
                    ));
                }
                self.push_block(anchor, &payloads, BlockMode::Replace);
            }
            BlockTarget::InsertAfterBlock { anchor } => {
                if payloads.is_empty() {
                    self.warnings
                        .push(format!("line {line_num}: {EMPTY_INSERT}"));
                }
                self.push_block(anchor, &payloads, BlockMode::InsertAfter);
            }
            BlockTarget::Replace { range } => {
                if payloads.is_empty() {
                    for anchor in expand_range(&range) {
                        self.push_delete(anchor);
                    }
                } else {
                    let cursor = Cursor::BeforeAnchor(Anchor { line: range.start });
                    self.emit_payload_rows(cursor, &payloads, InsertMode::Replacement);
                    for anchor in expand_range(&range) {
                        self.push_delete(anchor);
                    }
                }
            }
            BlockTarget::InsertBefore { anchor } => {
                if payloads.is_empty() {
                    self.warnings
                        .push(format!("line {line_num}: {EMPTY_INSERT}"));
                }
                self.emit_payload_rows(
                    Cursor::BeforeAnchor(anchor),
                    &payloads,
                    InsertMode::Insert,
                );
            }
            BlockTarget::InsertAfter { anchor } => {
                if payloads.is_empty() {
                    self.warnings
                        .push(format!("line {line_num}: {EMPTY_INSERT}"));
                }
                self.emit_payload_rows(
                    Cursor::AfterAnchor(anchor),
                    &payloads,
                    InsertMode::Insert,
                );
            }
            BlockTarget::Bof => {
                if payloads.is_empty() {
                    self.warnings
                        .push(format!("line {line_num}: {EMPTY_INSERT}"));
                }
                self.emit_payload_rows(Cursor::Bof, &payloads, InsertMode::Insert);
            }
            BlockTarget::Eof => {
                if payloads.is_empty() {
                    self.warnings
                        .push(format!("line {line_num}: {EMPTY_INSERT}"));
                }
                self.emit_payload_rows(Cursor::Eof, &payloads, InsertMode::Insert);
            }
            BlockTarget::Rem | BlockTarget::Move { .. } => {
                // Handled at hunk-header time; a pending Rem/Move should never
                // reach flush. No-op defensively.
            }
        }
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Parse a diff body (no section header) into a flat list of edits.
pub fn parse_patch(diff: &str) -> ParseResult {
    let tokens = tokenize(diff);
    let mut executor = Executor::new();
    for token in &tokens {
        executor.feed(token);
    }
    executor.end()
}

// ── Section splitting (port of input.ts splitRawSections) ─────────────────

/// Strip apply_patch-style noise that models prepend to the path. Examples:
/// `Update File:foo.ts`, `Update:foo.ts`, `***foo.ts`, `***Update File:foo.ts`.
/// Strips a leading `***`, then an optional `(Update|Add|Delete|Move)[<sep>]*(File|to)?[<sep>]*:`
/// keyword block (case-insensitive), then another optional `***`.
fn strip_apply_patch_path_noise(path_text: &str) -> String {
    let mut s = path_text;
    // Leading `***` (model duplicating the header sigil).
    while s.starts_with('*') {
        s = &s[1..];
    }
    s = s.trim_start();
    // Optional keyword block: (update|add|delete|move) ... ':' (case-insensitive).
    // The TS regex `(?:update|...)[^A-Za-z0-9]*(?:file|to)?[^A-Za-z0-9]*:` relies on
    // backtracking to leave the trailing `:` for the literal; RE2 and manual scans
    // can't backtrack, so we scan for the first `:` after the keyword and treat
    // everything up to and including it as noise.
    let lower = s.to_ascii_lowercase();
    let kw = ["update", "add", "delete", "move"]
        .iter()
        .find_map(|&k| lower.starts_with(k).then(|| k));
    if let Some(kw) = kw {
        let after_kw = &s[kw.len()..];
        // The keyword block must be followed (through non-alphanumeric separators)
        // by an optional `file`/`to`, then non-alphanumeric separators, then `:`.
        // Find the first `:` — but only accept it if the run between the keyword
        // and the `:` is entirely non-alphanumeric (optionally containing `file`/`to`).
        if let Some(colon_idx) = after_kw.find(':') {
            let between = &after_kw[..colon_idx];
            // Consume leading non-alphanumeric separators, then an optional
            // `file`/`to`, then trailing non-alphanumeric separators — matching
            // the TS `[^A-Za-z0-9]*(?:file|to)?[^A-Za-z0-9]*` (with backtracking
            // to leave the `:` for the literal).
            let lead_trimmed = between.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
            let lower_lead = lead_trimmed.to_ascii_lowercase();
            let after_word = if lower_lead.starts_with("file") {
                &lead_trimmed[4..]
            } else if lower_lead.starts_with("to") {
                &lead_trimmed[2..]
            } else {
                lead_trimmed
            };
            if after_word
                .bytes()
                .all(|b| !b.is_ascii_alphanumeric())
            {
                let after_colon = &after_kw[colon_idx + 1..];
                s = after_colon.trim_start();
                while s.starts_with('*') {
                    s = &s[1..];
                }
                s = s.trim_start();
            }
        }
    }
    s.to_string()
}

fn unquote_hashline_path(path_text: &str) -> String {
    let bytes = path_text.as_bytes();
    if bytes.len() < 2 {
        return path_text.to_string();
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if (first == b'"' || first == b'\'') && first == last {
        path_text[1..bytes.len() - 1].to_string()
    } else {
        path_text.to_string()
    }
}

fn normalize_hashline_path(raw_path: &str) -> String {
    let unquoted = strip_apply_patch_path_noise(&unquote_hashline_path(raw_path.trim()));
    unquoted
}

#[derive(Debug, Clone)]
struct RawSection {
    path: String,
    file_hash: Option<String>,
    diff: String,
}

/// Best-effort recovery for bracketed header lines the strict tokenizer
/// rejects. Strips apply_patch keyword noise and an extra leading `***`,
/// then expects `PATH(#HASH)?`.
fn try_parse_recovery_header(line: &str) -> Option<RawSection> {
    if !line.starts_with(HL_FILE_PREFIX) || !line.ends_with(HL_FILE_SUFFIX) {
        return None;
    }
    let inner = &line[HL_FILE_PREFIX.len()..line.len() - HL_FILE_SUFFIX.len()];
    let body = strip_apply_patch_path_noise(inner.trim());
    if body.is_empty() {
        return None;
    }

    // Trailing `#XXXX` is the tag; everything before it is the path. The path
    // may contain whitespace, so anchor the tag at end-of-body.
    let trailing_re = Regex::new(&format!(
        r"#([0-9A-Fa-f]{{{}}})\s*$",
        HL_FILE_HASH_LENGTH
    ))
    .unwrap();
    let (path_text, file_hash) = if let Some(caps) = trailing_re.captures(&body) {
        let hash = caps[1].to_ascii_uppercase();
        let path = &body[..caps.get(0).unwrap().start()];
        (path.to_string(), Some(hash))
    } else {
        (body.trim_end().to_string(), None)
    };

    // No `#` allowed in the path body.
    if path_text.contains('#') {
        return None;
    }
    let path = normalize_hashline_path(&path_text);
    if path.is_empty() {
        return None;
    }
    Some(RawSection {
        path,
        file_hash,
        diff: String::new(),
    })
}

/// Parse a `[PATH]` or `[PATH#hash]` header line. Returns `Ok(Some)` for a
/// valid header, `Ok(None)` for lines that don't start with `[`, and
/// `Err(message)` when a bracketed line fails the strict shape.
fn parse_hashline_header_line(line: &str) -> Result<Option<RawSection>, String> {
    let trimmed = line.trim_end();
    if !trimmed.starts_with(HL_FILE_PREFIX) {
        return Ok(None);
    }
    let token = crate::tokenizer::tokenize_line(trimmed, 1);
    match token {
        Token::SectionHeader { path, file_hash, .. } => {
            let parsed_path = normalize_hashline_path(&path);
            if parsed_path.is_empty() {
                return Err(format!(
                    "Input header `{HL_FILE_PREFIX}{HL_FILE_SUFFIX}` is empty; provide a file path."
                ));
            }
            Ok(Some(RawSection {
                path: parsed_path,
                file_hash,
                diff: String::new(),
            }))
        }
        _ => {
            if let Some(recovered) = try_parse_recovery_header(trimmed) {
                return Ok(Some(recovered));
            }
            Err(format!(
                "Input header must be {HL_FILE_PREFIX}PATH{HL_FILE_SUFFIX} or \
                 {HL_FILE_PREFIX}PATH{HL_FILE_HASH_SEP}TAG{HL_FILE_SUFFIX} with a \
                 {HL_FILE_HASH_LENGTH}-hex content-hash tag; got {trimmed:?}."
            ))
        }
    }
}

fn strip_leading_blank_lines(input: &str) -> String {
    let stripped = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut lines: Vec<&str> = stripped.split('\n').collect();
    while let Some(head) = lines.first() {
        let head = head.strip_suffix('\r').unwrap_or(head);
        if head.trim().is_empty() || is_envelope_marker(head) {
            lines.remove(0);
            continue;
        }
        break;
    }
    lines.join("\n")
}

fn contains_recognizable_hashline_operations(input: &str) -> bool {
    input.split("\r\n").flat_map(|s| s.split('\n')).any(is_op)
}

fn normalize_fallback_input(input: &str, options: &SplitOptions) -> String {
    let stripped = input.strip_prefix('\u{feff}').unwrap_or(input);
    let has_explicit_header = stripped
        .split("\r\n")
        .flat_map(|s| s.split('\n'))
        .any(|raw_line| parse_hashline_header_line(raw_line).ok().flatten().is_some());
    if has_explicit_header {
        return input.to_string();
    }
    let Some(fallback_path) = &options.path else {
        return input.to_string();
    };
    if !contains_recognizable_hashline_operations(input) {
        return input.to_string();
    }
    let fallback = normalize_hashline_path(fallback_path);
    if fallback.is_empty() {
        return input.to_string();
    }
    format!("{HL_FILE_PREFIX}{fallback}{HL_FILE_SUFFIX}\n{input}")
}

fn split_raw_sections(input: &str, options: &SplitOptions) -> Result<Vec<RawSection>, String> {
    let stripped = strip_leading_blank_lines(&normalize_fallback_input(input, options));
    let lines: Vec<String> = split_hashline_lines(&stripped);
    let first_line = lines.first().map(|s| s.as_str()).unwrap_or("");

    if parse_hashline_header_line(first_line)?.is_none() {
        let first_trimmed = first_line.trim_end();
        let unified_re =
            Regex::new(r"^@@\s+[-+]?\d+,\d+\s+[-+]?\d+,\d+\s+@@").unwrap();
        if unified_re.is_match(first_trimmed) {
            return Err(format!(
                "unified-diff hunk header (`@@ -N,M +N,M @@`) is not valid in hashline. \
                 File sections start with `{HL_FILE_PREFIX}path{HL_FILE_HASH_SEP}HASH{HL_FILE_SUFFIX}`; \
                 use `SWAP`, `DEL`, or `INS` ops."
            ));
        }
        let preview_len = first_line.len().min(120);
        let preview = &first_line[..preview_len];
        return Err(format!(
            "input must begin with \"{HL_FILE_PREFIX}PATH{HL_FILE_HASH_SEP}HASH{HL_FILE_SUFFIX}\" \
             on the first non-blank line for anchored edits; got: {preview:?}. \
             Example: \"{HL_FILE_PREFIX}src/foo.ts{HL_FILE_HASH_SEP}1A2B{HL_FILE_SUFFIX}\" then edit ops."
        ));
    }

    let mut sections: Vec<RawSection> = Vec::new();
    let mut current: Option<RawSection> = None;
    let mut current_lines: Vec<String> = Vec::new();

    for line in &lines {
        let trimmed = line.trim_end();
        let token = crate::tokenizer::tokenize_line(line, 1);
        if matches!(token, Token::EnvelopeEnd { .. } | Token::Abort { .. }) {
            break;
        }
        if matches!(token, Token::EnvelopeBegin { .. }) {
            continue;
        }
        if trimmed.starts_with(HL_FILE_PREFIX) {
            let header = parse_hashline_header_line(line)?;
            if let Some(header) = header {
                flush_current(&mut current, &mut current_lines, &mut sections);
                current = Some(header);
                current_lines = Vec::new();
                continue;
            }
        }
        current_lines.push(line.clone());
    }
    flush_current(&mut current, &mut current_lines, &mut sections);
    Ok(sections)
}

fn flush_current(
    current: &mut Option<RawSection>,
    current_lines: &mut Vec<String>,
    sections: &mut Vec<RawSection>,
) {
    if let Some(mut section) = current.take() {
        let has_ops = current_lines.iter().any(|l| l.trim().len() > 0);
        if has_ops {
            section.diff = current_lines.join("\n");
            sections.push(section);
        }
    }
    current_lines.clear();
}

fn merge_same_path_sections(sections: Vec<RawSection>) -> Result<Vec<RawSection>, String> {
    let mut order: Vec<String> = Vec::new();
    let mut by_path: std::collections::HashMap<String, (Option<String>, Vec<String>)> =
        std::collections::HashMap::new();
    for section in sections {
        let entry = by_path.entry(section.path.clone()).or_insert_with(|| {
            order.push(section.path.clone());
            (None, Vec::new())
        });
        if let Some(new_hash) = &section.file_hash {
            if let Some(existing) = &entry.0 {
                if existing != new_hash {
                    return Err(format!(
                        "Conflicting hashline snapshot tags for {}: #{} and #{}. \
                         Re-read the file and retry with one current header.",
                        section.path, existing, new_hash
                    ));
                }
            } else {
                entry.0 = Some(new_hash.clone());
            }
        }
        entry.1.push(section.diff);
    }
    let mut merged = Vec::with_capacity(order.len());
    for path in order {
        let (file_hash, diffs) = by_path.remove(&path).unwrap();
        merged.push(RawSection {
            path,
            file_hash,
            diff: diffs.join("\n"),
        });
    }
    Ok(merged)
}

/// Parse a complete patch with `[path#TAG]` section headers into a list of
/// `PatchSection`s, each carrying its pre-parsed edits, file op, and warnings.
pub fn parse_sections(input: &str) -> Vec<PatchSection> {
    parse_sections_with_options(input, &SplitOptions::default())
}

/// Parse with explicit split options (cwd / fallback path).
pub fn parse_sections_with_options(
    input: &str,
    options: &SplitOptions,
) -> Vec<PatchSection> {
    let raw = match split_raw_sections(input, options) {
        Ok(raw) => raw,
        Err(_e) => return Vec::new(),
    };
    let merged = match merge_same_path_sections(raw) {
        Ok(merged) => merged,
        Err(_e) => return Vec::new(),
    };
    merged
        .into_iter()
        .map(|section| {
            let result = parse_patch(&section.diff);
            PatchSection {
                path: section.path,
                file_hash: section.file_hash,
                edits: result.edits,
                file_op: result.file_op,
                warnings: result.warnings,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::apply_edits;

    const FILE: &str = "a\nb\nc\nd\ne";

    fn apply_patch(text: &str, diff: &str) -> String {
        apply_edits(text, &parse_patch(diff).edits).text
    }

    #[test]
    fn canonical_replace_delete_insert() {
        assert_eq!(apply_patch(FILE, "SWAP 2.=3:\n+X"), "a\nX\nd\ne");
        assert_eq!(apply_patch(FILE, "DEL 2.=3"), "a\nd\ne");
        assert_eq!(apply_patch(FILE, "INS.PRE 2:\n+X"), "a\nX\nb\nc\nd\ne");
        assert_eq!(apply_patch(FILE, "INS.POST 2:\n+X"), "a\nb\nX\nc\nd\ne");
        assert_eq!(apply_patch(FILE, "INS.HEAD:\n+X"), "X\na\nb\nc\nd\ne");
        assert_eq!(apply_patch(FILE, "INS.TAIL:\n+X"), "a\nb\nc\nd\ne\nX");
    }

    #[test]
    fn single_number_shorthand() {
        assert_eq!(apply_patch(FILE, "SWAP 2:\n+X"), "a\nX\nc\nd\ne");
        assert_eq!(apply_patch(FILE, "DEL 2"), "a\nc\nd\ne");
    }

    #[test]
    fn alternate_range_separators_and_missing_colon() {
        assert_eq!(apply_patch(FILE, "SWAP 2-3:\n+X"), "a\nX\nd\ne");
        assert_eq!(apply_patch(FILE, "SWAP 2\u{2026}3:\n+X"), "a\nX\nd\ne");
        assert_eq!(apply_patch(FILE, "SWAP 2 3:\n+X"), "a\nX\nd\ne");
        assert_eq!(apply_patch(FILE, "SWAP 2..3:\n+X"), "a\nX\nd\ne");
        assert_eq!(apply_patch(FILE, "SWAP 2.=3\n+X"), "a\nX\nd\ne");
    }

    #[test]
    fn missing_colon_on_insert_headers() {
        assert_eq!(apply_patch(FILE, "INS.PRE 2\n+X"), "a\nX\nb\nc\nd\ne");
        assert_eq!(apply_patch(FILE, "INS.HEAD\n+X"), "X\na\nb\nc\nd\ne");
    }

    #[test]
    fn stray_dot_tolerance() {
        assert_eq!(apply_patch(FILE, "SWAP 2.=3.:\n+X"), "a\nX\nd\ne");
        assert_eq!(apply_patch(FILE, "SWAP 2.=2.:\n+X"), "a\nX\nc\nd\ne");
        assert_eq!(apply_patch(FILE, "INS.POST 2.:\n+X"), "a\nb\nX\nc\nd\ne");
        assert_eq!(apply_patch(FILE, "INS.PRE 2.:\n+X"), "a\nX\nb\nc\nd\ne");
        assert_eq!(apply_patch(FILE, "DEL 2.=3."), "a\nd\ne");
        assert_eq!(apply_patch(FILE, "INS.HEAD.:\n+X"), "X\na\nb\nc\nd\ne");
        assert_eq!(apply_patch(FILE, "INS.TAIL.:\n+X"), "a\nb\nc\nd\ne\nX");
    }

    #[test]
    fn auto_pipe_bare_body_row_with_warning() {
        let result = parse_patch("SWAP 2.=2:\n  hello");
        assert_eq!(apply_edits(FILE, &result.edits).text, "a\n  hello\nc\nd\ne");
        assert!(result.warnings.iter().any(|w| w.contains("Auto-prefixed bare body row")));
    }

    #[test]
    fn strips_read_output_prefix_from_bare_body_rows() {
        let result = parse_patch("SWAP 2.=2:\n2:hello");
        assert_eq!(apply_edits(FILE, &result.edits).text, "a\nhello\nc\nd\ne");
        assert!(result.warnings.iter().any(|w| w.contains("Auto-prefixed")));
    }

    #[test]
    fn preserves_plus_literal_payloads_without_stripping() {
        let result = parse_patch("SWAP 2.=2:\n+3:keep");
        assert_eq!(apply_edits(FILE, &result.edits).text, "a\n3:keep\nc\nd\ne");
        assert!(!result.warnings.iter().any(|w| w.contains("Auto-prefixed")));
    }

    #[test]
    fn strips_only_one_n_prefix() {
        let result = parse_patch("SWAP 2.=2:\n2:42:hello");
        assert_eq!(apply_edits(FILE, &result.edits).text, "a\n42:hello\nc\nd\ne");
    }

    #[test]
    fn strips_n_prefixes_only_when_all_bare_rows_carry_one() {
        let result = parse_patch("SWAP 2.=3:\n2:foo\n3:bar");
        assert_eq!(apply_edits(FILE, &result.edits).text, "a\nfoo\nbar\nd\ne");
    }

    #[test]
    fn leaves_bare_rows_untouched_when_only_some_carry_prefix() {
        let result = parse_patch("SWAP 2.=3:\n3:keep\nplain");
        assert_eq!(apply_edits(FILE, &result.edits).text, "a\n3:keep\nplain\nd\ne");
    }

    #[test]
    fn keeps_interior_blank_rows_in_bare_replace_body() {
        let result = parse_patch("SWAP 2.=3:\nfoo\n\nbar");
        assert_eq!(apply_edits(FILE, &result.edits).text, "a\nfoo\n\nbar\nd\ne");
    }

    #[test]
    fn drops_trailing_blank_rows_between_hunks() {
        let result = parse_patch("SWAP 2.=2:\nfoo\n\nSWAP 4.=4:\nbaz");
        assert_eq!(apply_edits(FILE, &result.edits).text, "a\nfoo\nc\nbaz\ne");
    }

    #[test]
    fn skips_blank_rows_when_checking_prefix_uniformity() {
        let result = parse_patch("SWAP 2.=3:\n2:foo\n\n3:bar");
        assert_eq!(apply_edits(FILE, &result.edits).text, "a\nfoo\n\nbar\nd\ne");
    }

    #[test]
    fn leaves_numeric_keyed_literal_bodies_untouched() {
        let result = parse_patch("SWAP 2.=3:\n1: \"one\",\n2: \"two\",");
        assert_eq!(
            apply_edits(FILE, &result.edits).text,
            "a\n1: \"one\",\n2: \"two\",\nd\ne"
        );
    }

    #[test]
    fn rejects_minus_body_rows() {
        let result = parse_patch("SWAP 2.=2:\n-old\n+new");
        assert!(result.warnings.iter().any(|w| w.contains("`-` rows are not valid")));
    }

    #[test]
    fn allows_literal_markdown_bullets_with_plus_prefix() {
        assert_eq!(
            apply_patch(FILE, "SWAP 2.=2:\n+- item\n+  - nested\n++plus"),
            "a\n- item\n  - nested\n+plus\nc\nd\ne"
        );
    }

    #[test]
    fn empty_replace_is_delete() {
        assert_eq!(apply_patch(FILE, "SWAP 2.=2:"), "a\nc\nd\ne");
    }

    #[test]
    fn empty_insert_warns() {
        let result = parse_patch("INS.TAIL:");
        assert!(result.warnings.iter().any(|w| w.contains("`INS` needs")));
    }

    #[test]
    fn delete_with_body_warns() {
        let result = parse_patch("DEL 2\n+X");
        assert!(result.warnings.iter().any(|w| w.contains("does not take body rows")));
    }

    #[test]
    fn delete_with_colon_warns() {
        let result = parse_patch("DEL 2:\n+X");
        assert!(result.warnings.iter().any(|w| w.contains("has no colon")));
    }

    #[test]
    fn abort_terminates_parsing() {
        let diff = "INS.POST 1:\n+HELLO\n*** Abort\nINS.POST 99:\n+never";
        let result = parse_patch(diff);
        assert_eq!(result.edits.len(), 1);
        assert_eq!(
            match &result.edits[0] {
                Edit::Insert { text, .. } => text.clone(),
                _ => String::new(),
            },
            "HELLO"
        );
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn bare_number_hunk_header_warns_with_verb_guidance() {
        let result = parse_patch("2\n+B");
        assert!(result.warnings.iter().any(|w| w.contains("hunk headers need a verb")));
    }

    #[test]
    fn bare_numeric_range_warns_with_verb_guidance() {
        let result = parse_patch("2 3\n+X");
        assert!(result.warnings.iter().any(|w| w.contains("Hunk headers need a verb")));
    }

    #[test]
    fn apply_patch_sentinel_rejected() {
        let result = parse_patch("*** Update File: a.ts\nSWAP 2.=2:\n+X");
        assert!(result.warnings.iter().any(|w| w.contains("apply_patch sentinel")));
    }

    #[test]
    fn unified_diff_hunk_header_rejected() {
        let result = parse_patch("@@ -1,3 +1,3 @@\nSWAP 2.=2:\n+X");
        assert!(result.warnings.iter().any(|w| w.contains("unified-diff hunk header")));
    }

    #[test]
    fn orphan_literal_payload_warns() {
        let result = parse_patch("+const X = 1;\nSWAP 2.=2:");
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("payload line has no preceding hunk header")));
    }

    #[test]
    fn block_modes_parsed_correctly() {
        let result = parse_patch("SWAP.BLK 5:\n+body");
        assert!(matches!(
            &result.edits[0],
            Edit::Block { mode: BlockMode::Replace, .. }
        ));
        let result = parse_patch("DEL.BLK 5");
        assert!(matches!(
            &result.edits[0],
            Edit::Block { mode: BlockMode::Delete, .. }
        ));
        let result = parse_patch("INS.BLK.POST 5:\n+body");
        assert!(matches!(
            &result.edits[0],
            Edit::Block { mode: BlockMode::InsertAfter, .. }
        ));
    }

    #[test]
    fn file_ops_parsed() {
        let result = parse_patch("REM");
        assert_eq!(result.file_op, Some(FileOp::Rem));
        let result = parse_patch("MV dest.ts");
        assert_eq!(result.file_op, Some(FileOp::Mv { dest: "dest.ts".into() }));
    }

    // ── parse_sections tests ──────────────────────────────────────────────

    #[test]
    fn parse_sections_extracts_path_tag_and_diff() {
        let input = "[src/foo.ts#1A2B]\nSWAP 2..2:\n+BBB";
        let sections = parse_sections(input);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "src/foo.ts");
        assert_eq!(sections[0].file_hash.as_deref(), Some("1A2B"));
        assert_eq!(apply_edits("aaa\nbbb\nccc", &sections[0].edits).text, "aaa\nBBB\nccc");
    }

    #[test]
    fn parse_sections_normalizes_leading_blanks() {
        let input = "\n[foo.ts]\nINS.HEAD:\n+x";
        let sections = parse_sections(input);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "foo.ts");
        assert_eq!(apply_edits("before", &sections[0].edits).text, "x\nbefore");
    }

    #[test]
    fn parse_sections_splits_multiple_and_drops_trailing_empty() {
        let input = "[a.ts]\nINS.HEAD:\n+a\n[b.ts]\nINS.TAIL:\n+b";
        let sections = parse_sections(input);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].path, "a.ts");
        assert_eq!(sections[1].path, "b.ts");

        let input = "[a.ts]\nINS.HEAD:\n+a\n[b.ts]";
        let sections = parse_sections(input);
        assert_eq!(sections.len(), 1);
    }

    #[test]
    fn parse_sections_abort_stops_before_later_sections() {
        let input = "[a.ts]\nINS.POST 1:\n+a-payload\n*** Abort\n[b.ts]\nINS.POST 1:\n+never";
        let sections = parse_sections(input);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "a.ts");
        assert_eq!(apply_edits("x", &sections[0].edits).text, "x\na-payload");
    }

    #[test]
    fn parse_sections_recovers_apply_patch_contaminated_header() {
        let input = "[*** Update File: dir with spaces/file.ts#1A2B]\nSWAP 1.=1:\n+after";
        let sections = parse_sections(input);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "dir with spaces/file.ts");
        assert_eq!(sections[0].file_hash.as_deref(), Some("1A2B"));
    }

    #[test]
    fn parse_sections_empty_returns_empty_vec() {
        let sections = parse_sections("DEL 38.=40");
        assert!(sections.is_empty());
    }
}
