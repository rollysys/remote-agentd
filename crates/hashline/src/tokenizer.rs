//! Tokenizer — port of packages/hashline/src/tokenizer.ts
//!
//! Stateful, line-oriented classifier for hashline diff text.
//!
//! Format shape:
//! ```text
//! [path/to/file.ts#1A2B]
//! SWAP 5.=7:
//! +literal new line
//! ```

use crate::format::*;
use crate::types::{Anchor, Cursor, ParsedRange};

// ── Character codes ──────────────────────────────────────────────────────

const CHAR_LINE_FEED: u8 = b'\n';
const CHAR_CARRIAGE_RETURN: u8 = b'\r';
const CHAR_ZERO: u8 = b'0';
const CHAR_NINE: u8 = b'9';
const CHAR_HASH: u8 = b'#';
const CHAR_TAB: u8 = b'\t';
const CHAR_SPACE: u8 = b' ';
const CHAR_DOT: u8 = b'.';
const CHAR_HYPHEN: u8 = b'-';
const CHAR_EQUALS: u8 = b'=';
const CHAR_DOUBLE_QUOTE: u8 = b'"';
const CHAR_SINGLE_QUOTE: u8 = b'\'';

const CHAR_UPPER_A: u8 = b'A';
const CHAR_UPPER_F: u8 = b'F';
const CHAR_LOWER_A: u8 = b'a';
const CHAR_LOWER_F: u8 = b'f';

const CHAR_PAYLOAD_REPLACE: u8 = HL_PAYLOAD_REPLACE as u8;
const CHAR_COLON: u8 = HL_HEADER_COLON as u8;
const FILE_PREFIX_LENGTH: usize = HL_FILE_PREFIX.len();
const FILE_SUFFIX_LENGTH: usize = HL_FILE_SUFFIX.len();

// The ellipsis char used as an alternate range separator is U+2026.
const ELLIPSIS_CHAR: char = '\u{2026}';

// ── Low-level character helpers ──────────────────────────────────────────

fn is_digit_code(code: u8) -> bool {
    code >= CHAR_ZERO && code <= CHAR_NINE
}

fn is_non_zero_digit_code(code: u8) -> bool {
    code > CHAR_ZERO && code <= CHAR_NINE
}

fn is_hex_digit_code(code: u8) -> bool {
    is_digit_code(code)
        || (code >= CHAR_UPPER_A && code <= CHAR_UPPER_F)
        || (code >= CHAR_LOWER_A && code <= CHAR_LOWER_F)
}

fn is_whitespace_code(code: u8) -> bool {
    code == CHAR_SPACE || (code >= CHAR_TAB && code <= CHAR_CARRIAGE_RETURN)
}

/// Skip ASCII whitespace, returning the new index. Only treats basic ASCII
/// whitespace (space, tab, LF, CR, VT, FF) as whitespace — matching the TS
/// `isWhitespaceCode` range check.
fn skip_whitespace(line: &[u8], index: usize, end: usize) -> usize {
    let mut i = index;
    while i < end && is_whitespace_code(line[i]) {
        i += 1;
    }
    i
}

/// Index of the last non-whitespace byte + 1 (i.e. exclusive end after
/// trimming trailing whitespace).
fn trim_end_index(line: &[u8]) -> usize {
    let mut end = line.len();
    while end > 0 && is_whitespace_code(line[end - 1]) {
        end -= 1;
    }
    end
}

fn is_empty_line(line: &[u8]) -> bool {
    line.is_empty()
}

/// True when `line` (after trailing-whitespace trim) equals `marker` exactly.
fn marker_line_equals(line: &[u8], marker: &str) -> bool {
    let end = trim_end_index(line);
    end == marker.len() && &line[..end] == marker.as_bytes()
}

// ── Line splitting ───────────────────────────────────────────────────────

/// Split text into hashline lines: split on `\n`, stripping a trailing `\r`
/// from each line. An empty input yields a single empty line (matching the
/// TS `splitHashlineLines`).
pub fn split_hashline_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != CHAR_LINE_FEED {
            i += 1;
            continue;
        }
        let mut end = i;
        if end > start && bytes[end - 1] == CHAR_CARRIAGE_RETURN {
            end -= 1;
        }
        lines.push(text[start..end].to_string());
        i += 1;
        start = i;
    }
    if start < bytes.len() {
        let mut end = bytes.len();
        if end > start && bytes[end - 1] == CHAR_CARRIAGE_RETURN {
            end -= 1;
        }
        lines.push(text[start..end].to_string());
    }
    lines
}

/// Clone a cursor (anchors are `Copy`, so this is trivially the identity).
pub fn clone_cursor(cursor: &Cursor) -> Cursor {
    *cursor
}

// ── Number / range scanning ──────────────────────────────────────────────

struct NumberScan {
    line: u32,
    next_index: usize,
}

/// Scan a bare line number starting at `index`. Returns `None` if the first
/// char is not a non-zero digit. Accepts leading-zero-free numbers (`[1-9]\d*`),
/// matching the TS `scanLineNumber`.
fn scan_line_number(line: &[u8], index: usize, end: usize) -> Option<NumberScan> {
    if index >= end || !is_non_zero_digit_code(line[index]) {
        return None;
    }
    let mut line_number: u32 = 0;
    let mut next_index = index;
    while next_index < end {
        let code = line[next_index];
        if !is_digit_code(code) {
            break;
        }
        line_number = line_number
            .checked_mul(10)
            .and_then(|v| v.checked_add(u32::from(code - CHAR_ZERO)))
            .expect("line number overflow during scan");
        next_index += 1;
    }
    Some(NumberScan {
        line: line_number,
        next_index,
    })
}

/// Parse a bare line-number anchor. Throws (returns Err) on malformed input.
pub fn parse_lid(raw: &str, line_num: usize) -> anyhow::Result<Anchor> {
    let bytes = raw.as_bytes();
    let end = trim_end_index(bytes);
    let number_start = skip_whitespace(bytes, 0, end);
    let number = scan_line_number(bytes, number_start, end);
    match number {
        Some(n) if skip_whitespace(bytes, n.next_index, end) == end => Ok(Anchor { line: n.line }),
        _ => {
            let examples = describe_anchor_examples("119");
            Err(anyhow::anyhow!(
                "line {line_num}: expected a line number such as {examples}; got {raw:?}. Use {pfx}PATH{sep}hash{sfx} from your latest read for file-version binding.",
                line_num = line_num,
                examples = examples,
                raw = raw,
                pfx = HL_FILE_PREFIX,
                sep = HL_FILE_HASH_SEP,
                sfx = HL_FILE_SUFFIX,
            ))
        }
    }
}

struct RangeScan {
    range: ParsedRange,
    next_index: usize,
}

/// Scan the range separator after the first line number: one of `-`, `…`,
/// `..`, `.=`, or bare whitespace, possibly repeated and mixed. Returns the
/// index where the second line number begins, or `None` if no valid
/// separator + non-zero-digit was found.
///
/// The TS implementation treats whitespace itself as a valid separator when
/// it precedes a digit; we mirror that here (`consumed_separator` is set by
/// whitespace too).
fn scan_range_separator(line: &[u8], index: usize, end: usize) -> Option<usize> {
    let mut cursor = index;
    let mut consumed_separator = false;
    while cursor < end {
        let code = line[cursor];
        if is_whitespace_code(code) {
            cursor += 1;
            consumed_separator = true;
            continue;
        }
        if code == CHAR_HYPHEN {
            cursor += 1;
            consumed_separator = true;
            continue;
        }
        // U+2026 ellipsis is 3 bytes (0xE2 0x80 0xA6).
        if line[cursor..].starts_with(ELLIPSIS_CHAR.encode_utf8(&mut [0u8; 4]).as_bytes()) {
            cursor += ELLIPSIS_CHAR.len_utf8();
            consumed_separator = true;
            continue;
        }
        if code == CHAR_DOT && cursor + 1 < end && (line[cursor + 1] == CHAR_DOT || line[cursor + 1] == CHAR_EQUALS)
        {
            cursor += 2;
            consumed_separator = true;
            continue;
        }
        break;
    }
    if !consumed_separator {
        return None;
    }
    if cursor >= end || !is_non_zero_digit_code(line[cursor]) {
        return None;
    }
    Some(cursor)
}

/// Scan a full header range (`N`, `N..M`, `N.=M`, etc.) starting at `index`.
/// When `allow_single` is true, a bare `N` with no separator yields a
/// single-line range `{ start: N, end: N }`.
fn scan_header_range(line: &[u8], index: usize, end: usize, allow_single: bool) -> Option<RangeScan> {
    let number_start = skip_whitespace(line, index, end);
    let start = scan_line_number(line, number_start, end)?;
    let after_first = match scan_range_separator(line, start.next_index, end) {
        Some(idx) => idx,
        None => {
            if !allow_single {
                return None;
            }
            return Some(RangeScan {
                range: ParsedRange {
                    start: start.line,
                    end: start.line,
                },
                next_index: skip_whitespace(line, start.next_index, end),
            });
        }
    };
    let end_number = scan_line_number(line, after_first, end)?;
    Some(RangeScan {
        range: ParsedRange {
            start: start.line,
            end: end_number.line,
        },
        next_index: skip_whitespace(line, end_number.next_index, end),
    })
}

// ── Block targets ────────────────────────────────────────────────────────

/// A parsed hunk target — the concrete operation a hunk header describes,
/// resolved down to anchors/ranges but not yet turned into edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockTarget {
    /// `SWAP N.=M:` — replace a range with a body.
    Replace { range: ParsedRange },
    /// `SWAP.BLK N:` — block replace (tree-sitter resolved at apply time).
    Block { anchor: Anchor },
    /// `DEL N.=M` — delete a range.
    Delete { range: ParsedRange },
    /// `DEL.BLK N` — block delete.
    DeleteBlock { anchor: Anchor },
    /// `INS.PRE N:` — insert before line N.
    InsertBefore { anchor: Anchor },
    /// `INS.POST N:` — insert after line N.
    InsertAfter { anchor: Anchor },
    /// `INS.BLK.POST N:` — insert after the last line of a block.
    InsertAfterBlock { anchor: Anchor },
    /// `REM` — remove the whole file.
    Rem,
    /// `MV dest` — move/rename the file.
    Move { dest: String },
    /// `INS.HEAD:` — insert at beginning of file.
    Bof,
    /// `INS.TAIL:` — insert at end of file.
    Eof,
}

struct TargetScan {
    target: BlockTarget,
    next_index: usize,
}

/// Scan a keyword at `index`: the line must start with `keyword` at that
/// position, and the following byte (if any) must be whitespace, `:`, or `.`
/// — i.e. a word boundary, not a continuation of an identifier. Returns the
/// index just past the keyword, or `None`.
fn scan_keyword(line: &[u8], index: usize, end: usize, keyword: &str) -> Option<usize> {
    let kb = keyword.as_bytes();
    if line.len() < index + kb.len() || &line[index..index + kb.len()] != kb {
        return None;
    }
    let next = index + kb.len();
    if next < end {
        let code = line[next];
        if !is_whitespace_code(code) && code != CHAR_COLON && code != CHAR_DOT {
            return None;
        }
    }
    Some(next)
}

/// GLM 5.2 inserts a stray `.` between the line number/range and the trailing
/// `:` (e.g. `SWAP 2.=3.:`, `INS.POST 2.:`). A `.` is never valid syntax at
/// this position, so skip it when it precedes an optional `:` or end-of-line.
fn skip_stray_dot(line: &[u8], index: usize, end: usize) -> usize {
    if index < end && line[index] == CHAR_DOT {
        let after = skip_whitespace(line, index + 1, end);
        if after == end || line[after] == CHAR_COLON {
            return after;
        }
    }
    index
}

/// Consume an optional trailing colon (with surrounding whitespace and a
/// tolerated stray dot), returning the index after it (or the original
/// position if no colon was present).
fn consume_optional_colon(line: &[u8], index: usize, end: usize) -> usize {
    let cursor = skip_whitespace(line, index, end);
    let cursor = skip_stray_dot(line, cursor, end);
    if cursor < end && line[cursor] == CHAR_COLON {
        skip_whitespace(line, cursor + 1, end)
    } else {
        cursor
    }
}

/// Scan the `.PRE` / `.POST` / `.HEAD` / `.TAIL` target after `INS`.
fn scan_insert_target(line: &[u8], index: usize, end: usize) -> Option<TargetScan> {
    if index >= end || line[index] != CHAR_DOT {
        return None;
    }
    let cursor = skip_whitespace(line, index + 1, end);

    if let Some(before_end) = scan_keyword(line, cursor, end, HL_INSERT_BEFORE) {
        let anchor = scan_line_number(line, skip_whitespace(line, before_end, end), end)?;
        let next_index = consume_optional_colon(line, anchor.next_index, end);
        return Some(TargetScan {
            target: BlockTarget::InsertBefore {
                anchor: Anchor { line: anchor.line },
            },
            next_index,
        });
    }
    if let Some(after_end) = scan_keyword(line, cursor, end, HL_INSERT_AFTER) {
        let anchor = scan_line_number(line, skip_whitespace(line, after_end, end), end)?;
        let next_index = consume_optional_colon(line, anchor.next_index, end);
        return Some(TargetScan {
            target: BlockTarget::InsertAfter {
                anchor: Anchor { line: anchor.line },
            },
            next_index,
        });
    }
    if let Some(head_end) = scan_keyword(line, cursor, end, HL_INSERT_HEAD) {
        return Some(TargetScan {
            target: BlockTarget::Bof,
            next_index: consume_optional_colon(line, head_end, end),
        });
    }
    if let Some(tail_end) = scan_keyword(line, cursor, end, HL_INSERT_TAIL) {
        return Some(TargetScan {
            target: BlockTarget::Eof,
            next_index: consume_optional_colon(line, tail_end, end),
        });
    }
    None
}

fn unquote_path(path_text: &str) -> String {
    let bytes = path_text.as_bytes();
    if bytes.len() < 2 {
        return path_text.to_string();
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if (first == CHAR_DOUBLE_QUOTE || first == CHAR_SINGLE_QUOTE) && first == last {
        path_text[1..bytes.len() - 1].to_string()
    } else {
        path_text.to_string()
    }
}

/// Scan the destination of a `MV` keyword: either a quoted string or a bare
/// trimmed path extending to end-of-line.
fn scan_move_dest(line: &[u8], index: usize, end: usize) -> Option<String> {
    let cursor = skip_whitespace(line, index, end);
    if cursor >= end {
        return None;
    }
    let first = line[cursor];
    if first == CHAR_DOUBLE_QUOTE || first == CHAR_SINGLE_QUOTE {
        let quote = first;
        let mut next = cursor + 1;
        while next < end {
            let ch = line[next];
            // Handle backslash escape inside quotes.
            if ch == b'\\' && next + 1 < end {
                next += 2;
                continue;
            }
            if ch == quote {
                let after = skip_whitespace(line, next + 1, end);
                if after != end {
                    return None;
                }
                let slice = std::str::from_utf8(&line[cursor..next + 1]).ok()?;
                return Some(unquote_path(slice));
            }
            next += 1;
        }
        return None;
    }
    // Bare dest: take the rest, trimmed.
    let slice = std::str::from_utf8(&line[cursor..end]).ok()?;
    let trimmed = slice.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(unquote_path(trimmed))
    }
}

/// Scan the full hunk anchor: REM, MV, SWAP.BLK, SWAP, DEL.BLK, DEL,
/// INS.BLK.POST, INS(.PRE/.POST/.HEAD/.TAIL).
fn scan_hunk_anchor(line: &[u8], start: usize, end: usize) -> Option<TargetScan> {
    let cursor = skip_whitespace(line, start, end);

    if let Some(rem_end) = scan_keyword(line, cursor, end, HL_REM_KEYWORD) {
        let next = skip_whitespace(line, rem_end, end);
        if next != end {
            return None;
        }
        return Some(TargetScan {
            target: BlockTarget::Rem,
            next_index: next,
        });
    }
    if let Some(move_end) = scan_keyword(line, cursor, end, HL_MOVE_KEYWORD) {
        let dest = scan_move_dest(line, move_end, end)?;
        if dest.is_empty() {
            return None;
        }
        return Some(TargetScan {
            target: BlockTarget::Move { dest },
            next_index: end,
        });
    }

    // `SWAP.BLK N:` — resolve N to a tree-sitter block range at apply time.
    if let Some(replace_block_end) = scan_keyword(line, cursor, end, HL_REPLACE_BLOCK_KEYWORD) {
        let anchor = scan_line_number(line, skip_whitespace(line, replace_block_end, end), end)?;
        return Some(TargetScan {
            target: BlockTarget::Block {
                anchor: Anchor { line: anchor.line },
            },
            next_index: consume_optional_colon(line, anchor.next_index, end),
        });
    }
    if let Some(replace_end) = scan_keyword(line, cursor, end, HL_REPLACE_KEYWORD) {
        let range = scan_header_range(line, replace_end, end, true)?;
        return Some(TargetScan {
            target: BlockTarget::Replace { range: range.range },
            next_index: consume_optional_colon(line, range.next_index, end),
        });
    }

    // `DEL.BLK N` — resolve N to a tree-sitter block range at apply time
    // and delete its whole span. Like `DEL N.=M`, it takes no body and no
    // trailing colon.
    if let Some(delete_block_end) = scan_keyword(line, cursor, end, HL_DELETE_BLOCK_KEYWORD) {
        let anchor = scan_line_number(line, skip_whitespace(line, delete_block_end, end), end)?;
        let mut next = skip_whitespace(line, anchor.next_index, end);
        next = skip_stray_dot(line, next, end);
        if next < end && line[next] == CHAR_COLON {
            return None;
        }
        return Some(TargetScan {
            target: BlockTarget::DeleteBlock {
                anchor: Anchor { line: anchor.line },
            },
            next_index: next,
        });
    }
    if let Some(delete_end) = scan_keyword(line, cursor, end, HL_DELETE_KEYWORD) {
        let range = scan_header_range(line, delete_end, end, true)?;
        let mut next = skip_whitespace(line, range.next_index, end);
        next = skip_stray_dot(line, next, end);
        if next < end && line[next] == CHAR_COLON {
            return None;
        }
        return Some(TargetScan {
            target: BlockTarget::Delete { range: range.range },
            next_index: next,
        });
    }

    // `INS.BLK.POST N:` — insert after the last line of the tree-sitter
    // block at N.
    if let Some(insert_after_block_end) = scan_keyword(line, cursor, end, HL_INSERT_AFTER_BLOCK_KEYWORD) {
        let anchor = scan_line_number(line, skip_whitespace(line, insert_after_block_end, end), end)?;
        return Some(TargetScan {
            target: BlockTarget::InsertAfterBlock {
                anchor: Anchor { line: anchor.line },
            },
            next_index: consume_optional_colon(line, anchor.next_index, end),
        });
    }
    if let Some(insert_end) = scan_keyword(line, cursor, end, HL_INSERT_KEYWORD) {
        return scan_insert_target(line, insert_end, end);
    }
    None
}

struct ParsedHunkHeader {
    target: BlockTarget,
}

/// Try to parse a full line as a hunk header. Returns `None` if the line
/// doesn't parse cleanly to a target consuming the entire (trimmed) line.
fn try_parse_hunk_header(line: &str) -> Option<ParsedHunkHeader> {
    let bytes = line.as_bytes();
    let end = trim_end_index(bytes);
    let start = skip_whitespace(bytes, 0, end);
    if start >= end {
        return None;
    }
    let scan = scan_hunk_anchor(bytes, start, end)?;
    if scan.next_index != end {
        return None;
    }
    Some(ParsedHunkHeader {
        target: scan.target,
    })
}

struct ParsedHeader {
    path: String,
    file_hash: Option<String>,
}

/// Try to parse a `[path#HASH]` section header line. Returns `None` for any
/// line that doesn't start with `[` or fails the strict shape (malformed tags,
/// embedded `#`, etc.).
fn try_parse_header(line: &str) -> Option<ParsedHeader> {
    let bytes = line.as_bytes();
    if !bytes.starts_with(HL_FILE_PREFIX.as_bytes()) {
        return None;
    }
    let end = trim_end_index(bytes);
    if FILE_PREFIX_LENGTH + FILE_SUFFIX_LENGTH >= end {
        return None;
    }
    if !line[FILE_PREFIX_LENGTH..end].ends_with(HL_FILE_SUFFIX) {
        return None;
    }
    let body_end = end - FILE_SUFFIX_LENGTH;
    if FILE_PREFIX_LENGTH >= body_end {
        return None;
    }

    // The snapshot tag, when present, is the trailing `#XXXX` block inside
    // the bracketed header. Detect it from the suffix so the path may
    // legitimately contain whitespace.
    let mut path_end = body_end;
    let mut file_hash: Option<String> = None;
    let trailing_hash_start = body_end.saturating_sub(HL_FILE_HASH_LENGTH + 1);
    if trailing_hash_start >= FILE_PREFIX_LENGTH && bytes[trailing_hash_start] == CHAR_HASH {
        let mut all_hex = true;
        for probe in (trailing_hash_start + 1)..body_end {
            if !is_hex_digit_code(bytes[probe]) {
                all_hex = false;
                break;
            }
        }
        if all_hex {
            path_end = trailing_hash_start;
            file_hash = Some(
                line[trailing_hash_start + 1..body_end]
                    .to_ascii_uppercase(),
            );
        }
    }

    // The hashline header grammar uses `#` as the path/tag separator and
    // does not allow `#` inside filenames. Any `#` left in the path body
    // — short tags, non-hex tags, over-long tags, stale-tag copy-paste,
    // line-suffixed tags — means the header is malformed.
    for i in FILE_PREFIX_LENGTH..path_end {
        if bytes[i] == CHAR_HASH {
            return None;
        }
    }

    if path_end == FILE_PREFIX_LENGTH {
        return None;
    }
    let path = line[FILE_PREFIX_LENGTH..path_end].to_string();
    Some(ParsedHeader { path, file_hash })
}

// ── Tokens ───────────────────────────────────────────────────────────────

/// A classified diff line produced by the tokenizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A blank line.
    Blank { line_num: usize },
    /// `*** Begin Patch` — optional envelope start, silently consumed.
    EnvelopeBegin { line_num: usize },
    /// `*** End Patch` — envelope end, terminates parsing.
    EnvelopeEnd { line_num: usize },
    /// `*** Abort` — truncation sentinel, terminates parsing.
    Abort { line_num: usize },
    /// `[path#HASH]` section header.
    SectionHeader {
        line_num: usize,
        path: String,
        file_hash: Option<String>,
    },
    /// A parsed hunk header (`SWAP`, `DEL`, `INS`, `REM`, `MV`, …).
    HunkHeader { line_num: usize, target: BlockTarget },
    /// `+text` — a literal body row (the `+` prefix already stripped).
    BodyRow { line_num: usize, text: String },
    /// Any other raw line — becomes payload or an error in the parser.
    Raw { line_num: usize, text: String },
}

/// Classify a single line into a token.
fn classify_line(line: &str, line_num: usize) -> Token {
    let bytes = line.as_bytes();
    if is_empty_line(bytes) {
        return Token::Blank { line_num };
    }
    if marker_line_equals(bytes, BEGIN_PATCH_MARKER) {
        return Token::EnvelopeBegin { line_num };
    }
    if marker_line_equals(bytes, END_PATCH_MARKER) {
        return Token::EnvelopeEnd { line_num };
    }
    if marker_line_equals(bytes, ABORT_MARKER) {
        return Token::Abort { line_num };
    }
    if bytes.starts_with(HL_FILE_PREFIX.as_bytes()) {
        if let Some(header) = try_parse_header(line) {
            return Token::SectionHeader {
                line_num,
                path: header.path,
                file_hash: header.file_hash,
            };
        }
    }
    let lead = skip_whitespace(bytes, 0, bytes.len());
    let is_hunk_lead = line[lead..].starts_with(HL_REPLACE_KEYWORD)
        || line[lead..].starts_with(HL_DELETE_KEYWORD)
        || line[lead..].starts_with(HL_INSERT_KEYWORD)
        || line[lead..].starts_with(HL_REM_KEYWORD)
        || line[lead..].starts_with(HL_MOVE_KEYWORD);
    if is_hunk_lead {
        if let Some(hunk) = try_parse_hunk_header(line) {
            return Token::HunkHeader {
                line_num,
                target: hunk.target,
            };
        }
    }
    if !bytes.is_empty() && bytes[0] == CHAR_PAYLOAD_REPLACE {
        return Token::BodyRow {
            line_num,
            text: line[1..].to_string(),
        };
    }
    Token::Raw {
        line_num,
        text: line.to_string(),
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Marker constants (mirrors of messages.ts). Defined here so the tokenizer
/// is self-contained; the canonical home is messages.rs.
pub const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
pub const END_PATCH_MARKER: &str = "*** End Patch";
pub const ABORT_MARKER: &str = "*** Abort";

/// Tokenize a full diff string into a flat list of tokens, assigning line
/// numbers starting at 1. This is the Rust equivalent of the TS
/// `Tokenizer.tokenizeAll`.
pub fn tokenize(input: &str) -> Vec<Token> {
    let lines = split_hashline_lines(input);
    let mut tokens = Vec::with_capacity(lines.len());
    for (idx, line) in lines.into_iter().enumerate() {
        tokens.push(classify_line(&line, idx + 1));
    }
    tokens
}

/// Tokenize a single line with an explicit line number (TS `Tokenizer.tokenize`).
pub fn tokenize_line(line: &str, line_num: usize) -> Token {
    classify_line(line, line_num)
}

/// True when `line` parses as a recognized hashline hunk header (TS `isOp`).
pub fn is_op(line: &str) -> bool {
    try_parse_hunk_header(line).is_some()
}

/// True when `line` parses as a `[path#HASH]` section header (TS `isHeader`).
pub fn is_header(line: &str) -> bool {
    try_parse_header(line).is_some()
}

/// True when `line` is one of the envelope/abort markers (TS `isEnvelopeMarker`).
pub fn is_envelope_marker(line: &str) -> bool {
    let bytes = line.as_bytes();
    marker_line_equals(bytes, BEGIN_PATCH_MARKER)
        || marker_line_equals(bytes, END_PATCH_MARKER)
        || marker_line_equals(bytes, ABORT_MARKER)
}

/// Describe representative anchor examples for error messages (mirrors
/// `describeAnchorExamples` from format.ts).
fn describe_anchor_examples(line_prefix: &str) -> String {
    let examples: Vec<String> = if !line_prefix.is_empty() {
        let second = {
            let mut s = line_prefix.to_string();
            s.pop();
            let base = if s.is_empty() { "4" } else { &s };
            format!("{}2", base)
        };
        vec![line_prefix.to_string(), second, "7".to_string()]
    } else {
        vec!["160".to_string(), "42".to_string(), "7".to_string()]
    };
    examples
        .iter()
        .map(|e| format!("\"{}\"", e))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_blank_and_body() {
        let tokens = tokenize("SWAP 2.=2:\n+hello\n\n+world");
        assert!(matches!(tokens[0], Token::HunkHeader { .. }));
        assert!(matches!(tokens[1], Token::BodyRow { ref text, .. } if text == "hello"));
        assert!(matches!(tokens[2], Token::Blank { .. }));
        assert!(matches!(tokens[3], Token::BodyRow { ref text, .. } if text == "world"));
    }

    #[test]
    fn tokenize_section_header_with_tag() {
        let tokens = tokenize("[src/foo.ts#1A2B]\nSWAP 1.=1:\n+x");
        match &tokens[0] {
            Token::SectionHeader {
                path, file_hash, ..
            } => {
                assert_eq!(path, "src/foo.ts");
                assert_eq!(file_hash.as_deref(), Some("1A2B"));
            }
            other => panic!("expected SectionHeader, got {:?}", other),
        }
    }

    #[test]
    fn tokenize_envelope_markers() {
        let tokens = tokenize("*** Begin Patch\n*** End Patch\n*** Abort");
        assert!(matches!(tokens[0], Token::EnvelopeBegin { .. }));
        assert!(matches!(tokens[1], Token::EnvelopeEnd { .. }));
        assert!(matches!(tokens[2], Token::Abort { .. }));
    }

    #[test]
    fn tokenize_all_hunk_forms() {
        assert!(matches!(
            tokenize_line("SWAP 2.=3:", 1),
            Token::HunkHeader { target: BlockTarget::Replace { .. }, .. }
        ));
        assert!(matches!(
            tokenize_line("DEL 2.=3", 1),
            Token::HunkHeader { target: BlockTarget::Delete { .. }, .. }
        ));
        assert!(matches!(
            tokenize_line("INS.PRE 2:", 1),
            Token::HunkHeader { target: BlockTarget::InsertBefore { .. }, .. }
        ));
        assert!(matches!(
            tokenize_line("INS.HEAD:", 1),
            Token::HunkHeader { target: BlockTarget::Bof, .. }
        ));
        assert!(matches!(
            tokenize_line("SWAP.BLK 5:", 1),
            Token::HunkHeader { target: BlockTarget::Block { .. }, .. }
        ));
        assert!(matches!(
            tokenize_line("DEL.BLK 5", 1),
            Token::HunkHeader { target: BlockTarget::DeleteBlock { .. }, .. }
        ));
        assert!(matches!(
            tokenize_line("INS.BLK.POST 5:", 1),
            Token::HunkHeader { target: BlockTarget::InsertAfterBlock { .. }, .. }
        ));
        assert!(matches!(
            tokenize_line("REM", 1),
            Token::HunkHeader { target: BlockTarget::Rem, .. }
        ));
        assert!(matches!(
            tokenize_line("MV dest.ts", 1),
            Token::HunkHeader { target: BlockTarget::Move { .. }, .. }
        ));
    }

    #[test]
    fn tokenize_alternate_range_separators() {
        for header in &["SWAP 2-3:", "SWAP 2..3:", "SWAP 2 3:", "SWAP 2.=3:", "SWAP 2\u{2026}3:"] {
            let t = tokenize_line(header, 1);
            match &t {
                Token::HunkHeader {
                    target: BlockTarget::Replace { range },
                    ..
                } => {
                    assert_eq!(range.start, 2, "header: {}", header);
                    assert_eq!(range.end, 3, "header: {}", header);
                }
                other => panic!("{} -> {:?}", header, other),
            }
        }
    }

    #[test]
    fn tokenize_stray_dot_tolerance() {
        assert!(matches!(
            tokenize_line("SWAP 2.=3.:", 1),
            Token::HunkHeader { .. }
        ));
        assert!(matches!(
            tokenize_line("INS.POST 2.:", 1),
            Token::HunkHeader { .. }
        ));
        assert!(matches!(
            tokenize_line("DEL 2.=3.", 1),
            Token::HunkHeader { .. }
        ));
    }

    #[test]
    fn tokenize_rejects_malformed_header() {
        assert!(matches!(tokenize_line("[src/a.ts#1A2]", 1), Token::Raw { .. }));
        assert!(matches!(tokenize_line("[src/a.ts#1A2G]", 1), Token::Raw { .. }));
        assert!(matches!(tokenize_line("[src/a.ts#1A2B5]", 1), Token::Raw { .. }));
        // trailing junk after tag
        assert!(matches!(
            tokenize_line("[src/a.ts#1A2B copied from read]", 1),
            Token::Raw { .. }
        ));
    }

    #[test]
    fn tokenize_delete_with_colon_is_not_op() {
        // `DEL 2:` is not a valid hunk header — the tokenizer rejects it,
        // leaving it as Raw for the parser to surface the error.
        assert!(matches!(tokenize_line("DEL 2:", 1), Token::Raw { .. }));
    }
}
