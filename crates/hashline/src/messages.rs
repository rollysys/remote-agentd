//! Messages — port of packages/hashline/src/messages.ts
//!
//! Centralized error/warning text for the hashline parser, applier, and patcher.

use crate::format::{
    format_numbered_line, HL_FILE_HASH_SEP, HL_FILE_PREFIX, HL_FILE_SUFFIX, HL_RANGE_SEP,
};
use crate::types::BlockOp;

/// Lines of context shown either side of a hash mismatch.
pub const MISMATCH_CONTEXT: usize = 2;

/// Optional patch envelope start marker; silently consumed.
pub const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";

/// Optional patch envelope end marker; terminates parsing.
pub const END_PATCH_MARKER: &str = "*** End Patch";

/// Truncation sentinel emitted by an agent loop mid-call. Ends parsing like
/// `END_PATCH_MARKER`, without a warning.
pub const ABORT_MARKER: &str = "*** Abort";

/// Two consecutive hunks targeted the exact same concrete range.
pub const REPLACE_PAIR_COALESCED_WARNING: &str = "Two hunks targeted the same range; kept only the second. One `SWAP N.=M:` hunk per range — the body is the final content, never old+new.";

/// Bare bodyless hunk followed by an overlapping concrete hunk.
pub const BARE_OVERLAP_DROPPED_WARNING: &str = "Dropped a bare hunk overlapped by the concrete hunk after it. One `SWAP N.=M:` hunk per range — the body is the final content, never old+new.";

/// Bare body rows auto-converted to literal `+` rows.
pub const BARE_BODY_AUTO_PIPED_WARNING: &str =
    "Auto-prefixed bare body row(s) with `+`. Body rows must be `+TEXT` literal lines.";

/// Unified-diff-style `-` row in a hunk body.
pub const MINUS_ROW_REJECTED: &str = "`-` rows are not valid; the range already names the lines being changed. For Markdown bullets or other literal `-` lines, prefix the literal row with `+`: `+- item`.";

/// Replace hunk with no body.
pub fn empty_replace_message() -> String {
    format!("`SWAP N{HL_RANGE_SEP}M:` needs at least one `+TEXT` body row. To delete lines, use `DEL N{HL_RANGE_SEP}M`.")
}

/// `replace_block N:` hunk with no body.
pub fn empty_block_message() -> String {
    "`SWAP.BLK N:` needs at least one `+TEXT` body row. To delete a block, use `DEL.BLK N`.".to_string()
}

/// Block-anchored edit reached a path with no BlockResolver wired in — a host-configuration bug.
pub const BLOCK_RESOLVER_UNAVAILABLE: &str =
    "`SWAP.BLK`/`DEL.BLK`/`INS.BLK.POST` are not available here (no block resolver configured). Use a concrete line range.";

/// Internal invariant: `applyEdits` received an unresolved `replace_block N:` edit.
pub const UNRESOLVED_BLOCK_INTERNAL: &str =
    "internal error: unresolved `SWAP.BLK` edit reached the applier (resolveBlockEdits was not run).";

/// Delete hunk received a body row.
pub fn delete_takes_no_body_message() -> String {
    format!("`DEL N{HL_RANGE_SEP}M` does not take body rows. Remove the body, or use `SWAP N{HL_RANGE_SEP}M:`.")
}

/// `REM` received a body row or coexists with line edits.
pub const REM_TAKES_NO_BODY: &str =
    "`REM` deletes the whole file and takes no body rows or line ops. Issue it alone under the header.";

/// `MV` received a body row.
pub const MOVE_TAKES_NO_BODY: &str = "`MV DEST` does not take body rows. Put line edits above the `MV` row; the destination path follows `MV` on the same line.";

/// `delete_block N` hunk received a body row.
pub const DELETE_BLOCK_TAKES_NO_BODY: &str =
    "`DEL.BLK N` does not take body rows. Remove the body, or use `SWAP.BLK N:`.";

/// Insert hunk with no body.
pub const EMPTY_INSERT: &str = "`INS` needs at least one `+TEXT` body row.";

/// `Recovery`: an external write matched a cached snapshot.
pub const RECOVERY_EXTERNAL_WARNING: &str =
    "Recovered from a stale file hash using a previous read snapshot (file changed externally between read and edit).";

/// `Recovery`: a prior in-session edit advanced the hash.
pub const RECOVERY_SESSION_CHAIN_WARNING: &str =
    "Recovered from a stale file hash using an earlier in-session snapshot (a prior edit in this session advanced the hash).";

/// `Recovery`: session-chain replay fast-path.
pub const RECOVERY_SESSION_REPLAY_WARNING: &str = "Recovered by replaying your edits onto the current file content (a prior in-session edit changed the lines you re-targeted with a stale hash). Verify the diff matches your intent.";

/// `Recovery`: stale anchors were relocated to unchanged live lines after drift.
pub const RECOVERY_LINE_REMAP_WARNING: &str = "Recovered by remapping stale line anchors to unchanged current lines (file changed since the tagged read). Verify the diff matches your intent.";

/// `insert head:`/`insert tail:` applied despite a stale snapshot tag.
pub const HEADTAIL_DRIFT_WARNING: &str = "Applied the `INS.HEAD:`/`INS.TAIL:` edit despite a stale snapshot tag (file changed since your read) — head/tail position is content-independent. Re-read if the drift was unexpected.";


/// `insert_after_block N:` anchored on a closing-delimiter line, lowered to
/// plain `insert after N:`.
pub fn insert_after_block_closer_lowered_warning(line: u32) -> String {
    format!("`INS.BLK.POST {line}:` anchors on a closing delimiter, so it was applied as plain `INS.POST {line}:`. Anchor on the line that OPENS the construct.")
}

/// `insert_after_block N:` anchor unresolvable, lowered to plain `insert after N:`.
pub fn insert_after_block_unresolved_lowered_warning(line: u32) -> String {
    format!("`INS.BLK.POST {line}:` could not resolve a syntactic block on line {line}, so it was applied as plain `INS.POST {line}:`. Verify the landing line; anchor on a line that OPENS a construct.")
}

/// `insert after` body indented shallower than the anchor: the landing slid
/// forward past trailing closer lines.
pub fn after_insert_landing_shift_warning(anchor_line: u32, landing_line: u32, crossed: u32) -> String {
    let plural = if crossed == 1 { "" } else { "s" };
    format!("INS.POST {anchor_line}: body indented shallower than the anchor, so the landing moved past {crossed} closing line{plural} to after line {landing_line}. For the deeper position inside the block, re-issue with the body indented to match.")
}

/// `insert_after_block N:` body indented deeper than the block's closer: the
/// landing was pulled inside the block.
pub fn block_insert_landing_shift_warning(block_start: u32, closer_line: u32, landing_line: u32) -> String {
    format!("INS.BLK.POST {block_start}: body indented deeper than closing line {closer_line}, so it was placed inside the block, after line {landing_line}. `INS.BLK.POST` lands AFTER the block at sibling depth — if inside was intended, use plain `INS.POST {closer_line}:`.")
}

/// Section omitted the mandatory snapshot tag.
pub fn missing_snapshot_tag_message(section_path: &str) -> String {
    format!(
        "Missing hashline snapshot tag for {section_path}; use `{HL_FILE_PREFIX}{section_path}{HL_FILE_HASH_SEP}tag{HL_FILE_SUFFIX}` from your latest read/search output. To create a new file, use the write tool."
    )
}

/// A section named a path that does not exist, but its filename and snapshot
/// tag matched a file read earlier this session.
pub fn path_recovered_from_tag_message(authored_path: &str, resolved_path: &str, tag: &str) -> String {
    format!(
        "Path \"{authored_path}\" does not exist; matched its filename and snapshot tag {HL_FILE_HASH_SEP}{tag} to {resolved_path} (read earlier this session). Anchor future edits on {HL_FILE_PREFIX}{resolved_path}{HL_FILE_HASH_SEP}TAG{HL_FILE_SUFFIX}."
    )
}

/// Block-anchored replace/delete could not resolve to a syntactic block.
pub fn block_unresolved_message(line: u32, op: BlockOp, file_lines: Option<&[String]>) -> String {
    let op_word = match op {
        BlockOp::InsertAfter => "insert_after_block",
        BlockOp::Delete => "delete_block",
        BlockOp::Replace => "replace_block",
    };
    let base = format!(
        "`{op_word} {line}:` could not resolve a syntactic block beginning on line {line} (unsupported language, blank/out-of-range line, no node beginning on N, or parse error)."
    );
    match file_lines {
        Some(lines) => {
            let context = format_anchored_context(&[line], lines);
            if context.is_empty() {
                format!("{base} Anchor `{op_word}` on the line that OPENS the construct (e.g. its `function`/`if`/`case` header), never a statement inside it.")
            } else {
                format!("{base} Anchor `{op_word}` on the line that OPENS the construct (e.g. its `function`/`if`/`case` header), never a statement inside it.\n{}", context.join("\n"))
            }
        }
        None => format!("{base} Anchor `{op_word}` on the line that OPENS the construct (e.g. its `function`/`if`/`case` header), never a statement inside it."),
    }
}

/// A `replace_block`/`delete_block`/`insert_after_block` anchor resolved to a
/// single line — almost always a bare statement the model mis-anchored.
pub fn block_single_line_message(line: u32, op: BlockOp) -> String {
    let (block_form, plain_form) = match op {
        BlockOp::InsertAfter => ("INS.BLK.POST", format!("INS.POST {line}:")),
        BlockOp::Delete => ("DEL.BLK", format!("DEL {line}")),
        BlockOp::Replace => ("SWAP.BLK", format!("SWAP {line}{HL_RANGE_SEP}{line}:")),
    };
    format!(
        "`{block_form} {line}` resolved a single-line block — line {line} is a bare statement, not the opening line of a multi-line construct. For that one line use `{plain_form}`; to act on an enclosing construct, anchor {block_form} on the line that OPENS it (e.g. its `function`/`if`/`case` header), never a statement inside it."
    )
}

/// Numbered `LINE:TEXT` rows around `anchor_lines` (±`MISMATCH_CONTEXT`),
/// `*`-marking anchors, `...` between non-adjacent runs.
pub fn format_anchored_context(anchor_lines: &[u32], file_lines: &[String]) -> Vec<String> {
    use std::collections::BTreeSet;

    let mut display_lines: BTreeSet<u32> = BTreeSet::new();
    for &line in anchor_lines {
        if line < 1 || (line as usize) > file_lines.len() {
            continue;
        }
        let lo = line.saturating_sub(MISMATCH_CONTEXT as u32).max(1);
        let hi = (line + MISMATCH_CONTEXT as u32).min(file_lines.len() as u32);
        for line_num in lo..=hi {
            display_lines.insert(line_num);
        }
    }

    let anchor_set: std::collections::HashSet<u32> = anchor_lines.iter().copied().collect();
    let mut rows: Vec<String> = Vec::new();
    let mut previous: i64 = -1;
    for line_num in display_lines {
        if previous != -1 && (line_num as i64) > previous + 1 {
            rows.push("...".to_string());
        }
        previous = line_num as i64;
        let marker = if anchor_set.contains(&line_num) {
            "*"
        } else {
            " "
        };
        let text = file_lines.get((line_num - 1) as usize).map(|s| s.as_str()).unwrap_or("");
        rows.push(format!("{marker}{}", format_numbered_line(line_num as usize, text)));
    }
    rows
}

/// One anchored line whose actual content is being surfaced in an error message.
#[derive(Debug, Clone)]
pub struct RevealedLine {
    pub line: u32,
    pub text: String,
}

/// Content preview handed to `unseen_lines_message`.
#[derive(Debug, Clone, Default)]
pub struct UnseenLinesReveal {
    pub lines: Vec<RevealedLine>,
    pub truncated: bool,
}

/// Compress a line list into a sorted `1-4,7,10-12` range string.
fn format_line_ranges(lines: &[u32]) -> String {
    let mut sorted: Vec<u32> = lines.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut start = sorted[0];
    let mut prev = sorted[0];
    for i in 1..=sorted.len() {
        let current = sorted.get(i).copied();
        if current == Some(prev + 1) {
            prev = current.unwrap();
            continue;
        }
        parts.push(if start == prev {
            format!("{start}")
        } else {
            format!("{start}-{prev}")
        });
        if let Some(c) = current {
            start = c;
            prev = c;
        }
    }
    parts.join(", ")
}

/// An anchored edit referenced lines the read that minted the cited tag never
/// displayed.
pub fn unseen_lines_message(
    section_path: &str,
    unseen_lines: &[u32],
    tag: &str,
    reveal: &UnseenLinesReveal,
) -> String {
    let ranges = format_line_ranges(unseen_lines);
    let selector = ranges.replace(", ", ",");
    let header = format!(
        "This edit anchors to lines {ranges} of {section_path} that {HL_FILE_PREFIX}{section_path}{HL_FILE_HASH_SEP}{tag}{HL_FILE_SUFFIX} never displayed (it showed a partial range, a search hit, or a folded summary)."
    );
    if reveal.lines.is_empty() {
        return format!(
            "{header} Re-read them in full first with a ranged read like `{section_path}:{selector}` — it skips summarization and mints a fresh tag (a plain re-read just re-folds them) — then re-issue the edit."
        );
    }
    let preview: String = reveal
        .lines
        .iter()
        .map(|rl| format!("  {}", format_numbered_line(rl.line as usize, &rl.text)))
        .collect::<Vec<_>>()
        .join("\n");
    if reveal.truncated {
        return format!(
            "{header} Preview of the actual file content at the first {} unseen line(s):\n{preview}\nThe range exceeds the inline preview cap — re-read the remainder with `{section_path}:{selector}` before re-issuing the edit.",
            reveal.lines.len()
        );
    }
    format!(
        "{header} Actual file content at those lines:\n{preview}\nVerify the content matches what you intend to touch, then re-issue the edit with the same {HL_FILE_PREFIX}path{HL_FILE_HASH_SEP}tag{HL_FILE_SUFFIX} header — a straight retry now succeeds without a re-read. If the content does NOT match, fix your line numbers."
    )
}
