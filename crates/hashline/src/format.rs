//! Hashline format primitives: sigils, separators, constants.
//!
//! Direct port of packages/hashline/src/format.ts.
//! These MUST match the TS implementation exactly for tag compatibility.

// ── Sigils and delimiters ────────────────────────────────────────────────

pub const HL_FILE_PREFIX: &str = "[";
pub const HL_FILE_SUFFIX: &str = "]";
pub const HL_PAYLOAD_REPLACE: char = '+';
pub const HL_REPLACE_KEYWORD: &str = "SWAP";
pub const HL_DELETE_KEYWORD: &str = "DEL";
pub const HL_INSERT_KEYWORD: &str = "INS";
pub const HL_INSERT_BEFORE: &str = "PRE";
pub const HL_INSERT_AFTER: &str = "POST";
pub const HL_INSERT_HEAD: &str = "HEAD";
pub const HL_INSERT_TAIL: &str = "TAIL";
pub const HL_REPLACE_BLOCK_KEYWORD: &str = "SWAP.BLK";
pub const HL_DELETE_BLOCK_KEYWORD: &str = "DEL.BLK";
pub const HL_INSERT_AFTER_BLOCK_KEYWORD: &str = "INS.BLK.POST";
pub const HL_REM_KEYWORD: &str = "REM";
pub const HL_MOVE_KEYWORD: &str = "MV";
pub const HL_HEADER_COLON: char = ':';
pub const HL_FILE_HASH_SEP: char = '#';
pub const HL_RANGE_SEP: &str = ".=";
pub const HL_LINE_BODY_SEP: char = ':';

pub const HL_FILE_HASH_LENGTH: usize = 4;

/// Normalize text before hashing: trim trailing `[ \t\r]` from every line.
///
/// This is the exact same normalization as `normalizeFileHashText` in
/// `packages/hashline/src/format.ts`. It ensures CRLF endings and
/// display-trimmed lines do not invalidate a tag.
fn normalize_file_hash_text(text: &str) -> String {
    // Match the TS regex: /[ \t\r]+(?=\n|$)/g
    // This removes trailing whitespace before newlines and at end of string.
    let mut result = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let len = bytes.len();

    while i < len {
        // Find run of [ \t\r]
        if bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\r' {
            let start = i;
            while i < len && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\r') {
                i += 1;
            }
            // Check if followed by \n or end of string
            if i >= len || bytes[i] == b'\n' {
                // Skip this whitespace run (don't copy it)
                continue;
            } else {
                // Keep the whitespace
                result.push_str(&text[start..i]);
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    result
}

/// Compute the content-derived hash tag carried by a hashline section header.
///
/// The tag is a 4-hex fingerprint of the whole file's normalized text:
/// any read of byte-identical content mints the same tag, and a follow-up
/// edit anchored at any line validates whenever the live file still hashes to it.
///
/// Algorithm: xxHash32 of normalized text with seed=0, take low 16 bits,
/// format as 4 uppercase hex chars.
///
/// This MUST match `computeFileHash` in the TS implementation, which uses
/// `Bun.hash.xxHash32(normalized, 0) & 0xFFFF`.
pub fn compute_file_hash(text: &str) -> String {
    let normalized = normalize_file_hash_text(text);
    let hash = xxhash_rust::xxh32::xxh32(normalized.as_bytes(), 0);
    let low16 = hash & 0xFFFF;
    format!("{:04X}", low16)
}

/// Format a hashline section header for a file path and snapshot tag.
///
/// Example: `format_hashline_header("src/foo.ts", "1A2B")` → `"[src/foo.ts#1A2B]"`
pub fn format_hashline_header(file_path: &str, file_hash: &str) -> String {
    format!("{}{}{}{}", HL_FILE_PREFIX, file_path, HL_FILE_HASH_SEP, file_hash)
        + HL_FILE_SUFFIX
}

/// Format a single numbered line as `LINE:TEXT`.
pub fn format_numbered_line(line_number: usize, line: &str) -> String {
    format!("{}{}{}", line_number, HL_LINE_BODY_SEP, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_file_hash_basic() {
        // Same content → same tag
        let tag1 = compute_file_hash("hello\nworld\n");
        let tag2 = compute_file_hash("hello\nworld\n");
        assert_eq!(tag1, tag2);
        assert_eq!(tag1.len(), 4);
        assert!(tag1.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn test_compute_file_hash_normalizes_trailing_whitespace() {
        let tag1 = compute_file_hash("hello\nworld\n");
        let tag2 = compute_file_hash("hello   \nworld\t\r\n");
        assert_eq!(tag1, tag2);
    }

    #[test]
    fn test_compute_file_hash_different_content() {
        let tag1 = compute_file_hash("hello\n");
        let tag2 = compute_file_hash("world\n");
        assert_ne!(tag1, tag2);
    }

    #[test]
    fn test_format_hashline_header() {
        let header = format_hashline_header("src/foo.ts", "1A2B");
        assert_eq!(header, "[src/foo.ts#1A2B]");
    }

    #[test]
    fn test_format_numbered_line() {
        assert_eq!(format_numbered_line(1, "hello"), "1:hello");
        assert_eq!(format_numbered_line(42, "world"), "42:world");
    }
}
