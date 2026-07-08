//! Recovery — port of packages/hashline/src/recovery.ts
//!
//! Recover from a stale section snapshot tag by replaying the would-be edit
//! against the version the tag names and 3-way-merging the result onto the
//! live content, or by remapping stale anchors through the unchanged-line diff.

use std::collections::{HashMap, HashSet};

use similar::{ChangeTag, TextDiff};

use crate::apply::apply_edits;
use crate::messages::{
    RECOVERY_EXTERNAL_WARNING, RECOVERY_LINE_REMAP_WARNING, RECOVERY_SESSION_CHAIN_WARNING,
    RECOVERY_SESSION_REPLAY_WARNING,
};
use crate::snapshots::{InMemorySnapshotStore, Snapshot};
use crate::types::{Anchor, Cursor, Edit};

/// Section tags are line-precise; never let a patch slide a hunk onto a
/// duplicate closer 100+ lines away. If snapshot replay does not align
/// exactly, refuse and let the caller re-read.
const RECOVERY_FUZZ_FACTOR: usize = 0;

/// Arguments for a recovery attempt.
pub struct RecoveryArgs<'a> {
    pub path: &'a str,
    pub current_text: &'a str,
    pub file_hash: &'a str,
    pub edits: &'a [Edit],
}

/// Result of a successful recovery.
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    /// Post-recovery text.
    pub text: String,
    /// First changed line (1-indexed) relative to the live `current_text`, or `None`.
    pub first_changed_line: Option<u32>,
    /// Warnings collected during recovery, including the user-facing banner.
    pub warnings: Vec<String>,
}

/// Stateless recovery driver over a snapshot store. Construct once and call
/// `try_recover` per stale-tag incident.
pub struct Recovery<'a> {
    store: &'a InMemorySnapshotStore,
}

impl<'a> Recovery<'a> {
    pub fn new(store: &'a InMemorySnapshotStore) -> Self {
        Self { store }
    }

    /// Attempt recovery. Returns `None` when no path forward is found — the
    /// caller should then surface a `MismatchError`.
    pub fn try_recover(&self, args: RecoveryArgs<'_>) -> Option<RecoveryResult> {
        let RecoveryArgs {
            path,
            current_text,
            file_hash,
            edits,
        } = args;

        let snapshot = self.store.by_hash(path, file_hash)?;
        let is_head = self.is_head_snapshot(path, snapshot);
        let recovery_warning = if is_head {
            RECOVERY_EXTERNAL_WARNING
        } else {
            RECOVERY_SESSION_CHAIN_WARNING
        };

        // Strategy 1: 3-way merge — apply edits on the tagged snapshot, then
        // merge the resulting patch onto live content.
        if let Some(merged) = apply_edits_to_snapshot(
            &snapshot.content,
            current_text,
            edits,
            recovery_warning,
        ) {
            return Some(merged);
        }

        // Strategy 2: line-shift remap — remap anchors through the
        // unchanged-line diff, then replay on live content.
        if let Some(remapped) = replay_remapped_anchors_on_current(&snapshot.content, current_text, edits) {
            return Some(remapped);
        }

        // Strategy 3: session-chain replay — only for non-head snapshots.
        if !is_head {
            return replay_session_chain_on_current(&snapshot.content, current_text, edits);
        }

        None
    }

    /// True when `snapshot` is the most-recently recorded version for `path`.
    fn is_head_snapshot(&self, path: &str, snapshot: &Snapshot) -> bool {
        match self.store.head(path) {
            Some(head) => head.tag == snapshot.tag && head.content == snapshot.content,
            None => false,
        }
    }
}

// ── Strategy 1: 3-way merge ─────────────────────────────────────────────

fn apply_edits_to_snapshot(
    previous_text: &str,
    current_text: &str,
    edits: &[Edit],
    recovery_warning: &str,
) -> Option<RecoveryResult> {
    let applied = apply_edits(previous_text, edits);
    if applied.text == previous_text {
        return None;
    }

    let merged = structured_patch_apply(previous_text, &applied.text, current_text, 3, RECOVERY_FUZZ_FACTOR)?;
    if merged == current_text {
        return None;
    }

    let first_changed_line = find_first_changed_line(current_text, &merged).or(applied.first_changed_line);
    let has_net_change = first_changed_line.is_some();
    let mut warnings = if has_net_change {
        vec![recovery_warning.to_string()]
    } else {
        Vec::new()
    };
    warnings.extend(applied.warnings.iter().cloned());

    Some(RecoveryResult {
        text: merged,
        first_changed_line,
        warnings,
    })
}

// ── Strategy 2: line-shift remap ────────────────────────────────────────

fn replay_remapped_anchors_on_current(
    previous_text: &str,
    current_text: &str,
    edits: &[Edit],
) -> Option<RecoveryResult> {
    let remapped = remap_edits_to_current(previous_text, current_text, edits)?;
    let applied = apply_edits(current_text, &remapped);
    if applied.text == current_text {
        return None;
    }
    let mut warnings = vec![RECOVERY_LINE_REMAP_WARNING.to_string()];
    warnings.extend(applied.warnings.iter().cloned());
    Some(RecoveryResult {
        text: applied.text,
        first_changed_line: applied.first_changed_line,
        warnings,
    })
}

// ── Strategy 3: session-chain replay ────────────────────────────────────

fn replay_session_chain_on_current(
    previous_text: &str,
    current_text: &str,
    edits: &[Edit],
) -> Option<RecoveryResult> {
    // Guard 1: equal line counts.
    if previous_text.split('\n').count() != current_text.split('\n').count() {
        return None;
    }
    // Guard 2: anchor-content alignment.
    if !verify_anchor_content(previous_text, current_text, edits) {
        return None;
    }

    let applied = apply_edits(current_text, edits);
    if applied.text == current_text {
        return None;
    }
    let mut warnings = vec![RECOVERY_SESSION_REPLAY_WARNING.to_string()];
    warnings.extend(applied.warnings.iter().cloned());
    Some(RecoveryResult {
        text: applied.text,
        first_changed_line: applied.first_changed_line,
        warnings,
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Collect all anchor lines referenced by `edits`.
fn collect_anchor_lines(edits: &[Edit]) -> Vec<u32> {
    let mut lines = Vec::new();
    for edit in edits {
        for anchor in get_edit_anchors(edit) {
            lines.push(anchor.line);
        }
    }
    lines
}

fn get_edit_anchors(edit: &Edit) -> Vec<Anchor> {
    match edit {
        Edit::Delete { anchor } => vec![*anchor],
        Edit::Block { anchor, .. } => vec![*anchor],
        Edit::Insert { cursor, .. } => match cursor {
            Cursor::BeforeAnchor(a) | Cursor::AfterAnchor(a) => vec![*a],
            Cursor::Bof | Cursor::Eof => vec![],
        },
    }
}

/// True when every anchor line has identical content in both texts.
fn verify_anchor_content(previous_text: &str, current_text: &str, edits: &[Edit]) -> bool {
    let lines = collect_anchor_lines(edits);
    if lines.is_empty() {
        return true;
    }
    let prev: Vec<&str> = previous_text.split('\n').collect();
    let curr: Vec<&str> = current_text.split('\n').collect();
    for line in lines {
        let idx = (line - 1) as usize;
        if idx >= prev.len() || idx >= curr.len() {
            return false;
        }
        if prev[idx] != curr[idx] {
            return false;
        }
    }
    true
}

/// Build a map from previous-text line numbers (1-indexed) to current-text
/// line numbers, using a line-level LCS diff. Only unchanged lines get mapped.
fn build_line_map(previous_text: &str, current_text: &str) -> HashMap<u32, u32> {
    let previous_lines: Vec<&str> = previous_text.split('\n').collect();
    let current_lines: Vec<&str> = current_text.split('\n').collect();
    let diff = TextDiff::from_slices(&previous_lines, &current_lines);

    let mut map = HashMap::new();
    let mut previous_line: u32 = 0;
    let mut current_line: u32 = 0;

    for change in diff.iter_all_changes() {
        let count = 1u32;
        match change.tag() {
            ChangeTag::Equal => {
                previous_line += count;
                current_line += count;
                map.insert(previous_line, current_line);
            }
            ChangeTag::Delete => {
                previous_line += count;
            }
            ChangeTag::Insert => {
                current_line += count;
            }
        }
    }
    map
}

/// Values appearing two or more times in `lines`.
fn collect_duplicated_values(lines: &[&str]) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut duplicated = HashSet::new();
    for &value in lines {
        let owned = value.to_string();
        if !seen.insert(owned.clone()) {
            duplicated.insert(owned);
        }
    }
    duplicated
}

/// Nearest non-anchor context line on each side of an anchor run.
#[derive(Debug, Clone, Copy, Default)]
struct AnchorNeighbors {
    before: Option<u32>,
    after: Option<u32>,
}

/// Compute nearest non-anchor context for each anchor in one sweep over the
/// sorted anchor set.
fn compute_anchor_neighbors(anchor_lines: &HashSet<u32>, line_count: usize) -> HashMap<u32, AnchorNeighbors> {
    let mut sorted: Vec<u32> = anchor_lines.iter().copied().collect();
    sorted.sort_unstable();
    let mut neighbors = HashMap::new();
    let mut i = 0;
    while i < sorted.len() {
        let mut j = i;
        while j + 1 < sorted.len() && sorted[j + 1] == sorted[j] + 1 {
            j += 1;
        }
        let start = sorted[i];
        let end = sorted[j];
        let before = if start >= 2 && (start - 1) as usize <= line_count {
            Some(start - 1)
        } else {
            None
        };
        let after = if (end + 1) as usize <= line_count {
            Some(end + 1)
        } else {
            None
        };
        for k in i..=j {
            neighbors.insert(sorted[k], AnchorNeighbors { before, after });
        }
        i = j + 1;
    }
    neighbors
}

fn validate_duplicate_anchor_context(
    line: u32,
    mapped: u32,
    neighbors: AnchorNeighbors,
    line_map: &HashMap<u32, u32>,
) -> bool {
    let mut checked = false;
    if let Some(before) = neighbors.before {
        checked = true;
        // The line before the anchor should map to (mapped - (line - before)).
        let expected = mapped as i64 - (line as i64 - before as i64);
        if line_map.get(&before) != Some(&(expected as u32)) {
            return false;
        }
    }
    if let Some(after) = neighbors.after {
        checked = true;
        let expected = mapped as i64 + (after as i64 - line as i64);
        if line_map.get(&after) != Some(&(expected as u32)) {
            return false;
        }
    }
    checked
}

fn validate_unique_anchor_context(
    line: u32,
    mapped: u32,
    neighbors: AnchorNeighbors,
    line_map: &HashMap<u32, u32>,
) -> bool {
    let offset = mapped as i64 - line as i64;
    if let Some(after) = neighbors.after {
        let expected = (after as i64 + offset) as u32;
        return line_map.get(&after) == Some(&expected);
    }
    if let Some(before) = neighbors.before {
        let expected = (before as i64 + offset) as u32;
        return line_map.get(&before) == Some(&expected);
    }
    false
}

fn validate_remapped_anchor_context(
    previous_text: &str,
    current_text: &str,
    line_map: &HashMap<u32, u32>,
    edits: &[Edit],
) -> bool {
    let previous_lines: Vec<&str> = previous_text.split('\n').collect();
    let current_lines: Vec<&str> = current_text.split('\n').collect();
    let anchor_lines: HashSet<u32> = collect_anchor_lines(edits).into_iter().collect();

    let duplicated_previous = collect_duplicated_values(&previous_lines);
    let duplicated_current = collect_duplicated_values(&current_lines);
    let anchor_neighbors = compute_anchor_neighbors(&anchor_lines, previous_lines.len());

    for (&line, neighbors) in &anchor_neighbors {
        let mapped = match line_map.get(&line) {
            Some(&m) => m,
            None => return false,
        };
        let prev_idx = (line - 1) as usize;
        let curr_idx = (mapped - 1) as usize;

        let prev_val = previous_lines.get(prev_idx).map(|s| s.to_string());
        let curr_val = current_lines.get(curr_idx).map(|s| s.to_string());

        let prev_dup = prev_val.as_ref().map_or(false, |v| duplicated_previous.contains(v));
        let curr_dup = curr_val.as_ref().map_or(false, |v| duplicated_current.contains(v));

        if !prev_dup && !curr_dup {
            if !validate_unique_anchor_context(line, mapped, *neighbors, line_map) {
                return false;
            }
            continue;
        }
        if !validate_duplicate_anchor_context(line, mapped, *neighbors, line_map) {
            return false;
        }
    }
    true
}

/// Remap every stale anchor through the unchanged-line diff. Returns `None`
/// when any anchor can't be placed or the offsets aren't uniform.
fn remap_edits_to_current(
    previous_text: &str,
    current_text: &str,
    edits: &[Edit],
) -> Option<Vec<Edit>> {
    let line_map = build_line_map(previous_text, current_text);
    if !validate_remapped_anchor_context(previous_text, current_text, &line_map, edits) {
        return None;
    }

    let mut offsets: Vec<i64> = Vec::new();

    let map_line = |line: u32, offsets: &mut Vec<i64>| -> Option<u32> {
        let mapped = *line_map.get(&line)?;
        offsets.push(mapped as i64 - line as i64);
        Some(mapped)
    };

    let map_anchor = |anchor: &Anchor, offsets: &mut Vec<i64>| -> Option<Anchor> {
        let line = map_line(anchor.line, offsets)?;
        Some(Anchor::new(line))
    };

    let mut remapped = Vec::new();
    for edit in edits {
        match edit {
            Edit::Delete { anchor } => {
                let anchor = map_anchor(anchor, &mut offsets)?;
                remapped.push(Edit::Delete { anchor });
            }
            Edit::Block { anchor, payloads, mode } => {
                let anchor = map_anchor(anchor, &mut offsets)?;
                remapped.push(Edit::Block {
                    anchor,
                    payloads: payloads.clone(),
                    mode: *mode,
                });
            }
            Edit::Insert { cursor, text, mode: im } => {
                let new_cursor = match cursor {
                    Cursor::BeforeAnchor(a) => {
                        let a = map_anchor(a, &mut offsets)?;
                        Cursor::BeforeAnchor(a)
                    }
                    Cursor::AfterAnchor(a) => {
                        let a = map_anchor(a, &mut offsets)?;
                        Cursor::AfterAnchor(a)
                    }
                    Cursor::Bof | Cursor::Eof => *cursor,
                };
                remapped.push(Edit::Insert {
                    cursor: new_cursor,
                    text: text.clone(),
                    mode: *im,
                });
            }
        }
    }

    if offsets.is_empty() {
        return None;
    }
    let first_offset = offsets[0];
    if first_offset == 0 {
        return None;
    }
    if !offsets.iter().all(|&o| o == first_offset) {
        return None;
    }
    Some(remapped)
}

/// First 1-indexed line at which `a` and `b` diverge, or `None` if equal.
fn find_first_changed_line(a: &str, b: &str) -> Option<u32> {
    if a == b {
        return None;
    }
    let a_lines: Vec<&str> = a.split('\n').collect();
    let b_lines: Vec<&str> = b.split('\n').collect();
    let max = a_lines.len().max(b_lines.len());
    for i in 0..max {
        let av = a_lines.get(i).copied();
        let bv = b_lines.get(i).copied();
        if av != bv {
            return Some((i + 1) as u32);
        }
    }
    None
}

// ── Structured patch + apply (context=3, fuzz=0) ────────────────────────

/// A single hunk: lines to remove starting at `old_start`, and lines to insert.
struct PatchHunk {
    old_start: u32,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
}


/// Apply a structured patch (built from old→new) onto `target_text` with the
/// given fuzz factor. Returns `None` if any hunk cannot be placed exactly.
///
/// This mirrors `Diff.structuredPatch` + `Diff.applyPatch` with context and
/// fuzzFactor from the `diff` npm package.
fn structured_patch_apply(
    old_text: &str,
    new_text: &str,
    target_text: &str,
    context: usize,
    _fuzz: usize,
) -> Option<String> {
    let hunks = build_hunks_with_context(old_text, new_text, context);
    if hunks.is_empty() {
        return None;
    }

    let target_lines: Vec<&str> = target_text.split('\n').collect();
    let mut result: Vec<String> = Vec::new();
    let mut target_pos: usize = 0; // 0-indexed into target_lines

    for hunk in &hunks {
        let hunk_old_start_1 = hunk.old_start as usize; // 1-indexed
        let hunk_old_start_0 = hunk_old_start_1.saturating_sub(1);

        // Copy unchanged target lines before this hunk.
        while target_pos < hunk_old_start_0 {
            if target_pos >= target_lines.len() {
                // Hunk starts beyond target — can't place.
                return None;
            }
            result.push(target_lines[target_pos].to_string());
            target_pos += 1;
        }

        // Verify the context + removed lines match exactly (fuzz=0).
        // The hunk's old_lines are the lines being removed (the changed lines,
        // not the context). We verify those match the target at target_pos.
        for (i, old_line) in hunk.old_lines.iter().enumerate() {
            let target_idx = target_pos + i;
            if target_idx >= target_lines.len() {
                return None;
            }
            if target_lines[target_idx] != old_line.as_str() {
                return None;
            }
        }

        // Emit the new lines (replacement).
        for new_line in &hunk.new_lines {
            result.push(new_line.clone());
        }
        // Advance past the removed lines.
        target_pos += hunk.old_lines.len();
    }

    // Copy remaining target lines.
    while target_pos < target_lines.len() {
        result.push(target_lines[target_pos].to_string());
        target_pos += 1;
    }

    Some(result.join("\n"))
}

/// Build hunks with proper context lines, mirroring `Diff.structuredPatch`.
fn build_hunks_with_context(old_text: &str, new_text: &str, context: usize) -> Vec<PatchHunk> {
    let old_lines: Vec<&str> = old_text.split('\n').collect();
    let new_lines: Vec<&str> = new_text.split('\n').collect();
    let diff = TextDiff::from_slices(&old_lines, &new_lines);

    // Walk the diff, tracking old/new line positions, and build hunks.
    // A hunk captures: old_start (1-indexed), the full old block (context+removed+context),
    // and the new block (context+added+context).
    // For applyPatch, we only need old_start + removed lines + added lines.
    // But we must verify context too. So include context in old_lines/new_lines.

    #[derive(Clone)]
    struct DiffOp {
        tag: ChangeTag,
        old_line: Option<usize>, // 0-indexed old line
        #[allow(dead_code)]
        new_line: Option<usize>, // 0-indexed new line
        value: String,
    }

    let mut ops: Vec<DiffOp> = Vec::new();
    let mut old_idx: usize = 0;
    let mut new_idx: usize = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                ops.push(DiffOp {
                    tag: ChangeTag::Equal,
                    old_line: Some(old_idx),
                    new_line: Some(new_idx),
                    value: change.value().to_string(),
                });
                old_idx += 1;
                new_idx += 1;
            }
            ChangeTag::Delete => {
                ops.push(DiffOp {
                    tag: ChangeTag::Delete,
                    old_line: Some(old_idx),
                    new_line: None,
                    value: change.value().to_string(),
                });
                old_idx += 1;
            }
            ChangeTag::Insert => {
                ops.push(DiffOp {
                    tag: ChangeTag::Insert,
                    old_line: None,
                    new_line: Some(new_idx),
                    value: change.value().to_string(),
                });
                new_idx += 1;
            }
        }
    }

    // Find groups of changes (non-Equal ops) separated by gaps > 2*context.
    let mut hunks: Vec<PatchHunk> = Vec::new();
    let mut i = 0;
    while i < ops.len() {
        if ops[i].tag == ChangeTag::Equal {
            i += 1;
            continue;
        }
        // Start of a change group.
        let group_start = i;
        let mut j = i;
        // Extend the group: include changes and equal-runs shorter than 2*context+1.
        while j < ops.len() {
            if ops[j].tag != ChangeTag::Equal {
                j += 1;
                continue;
            }
            // Count the equal run.
            let eq_start = j;
            while j < ops.len() && ops[j].tag == ChangeTag::Equal {
                j += 1;
            }
            let eq_len = j - eq_start;
            // If this is the end of ops, or the gap is large, break.
            if eq_len > 2 * context {
                // The context after the last change is `context` lines from this run.
                break;
            }
            // Otherwise, the equal run is inside the hunk; continue.
        }
        let group_end = j; // exclusive

        // Now build the hunk: take context lines before group_start and after group_end.
        let hunk_first_change = &ops[group_start];
        let hunk_old_start_0 = hunk_first_change.old_line.unwrap_or_else(|| {
            // For a pure-insert at the start, old_line is None. Find the old-line
            // just before this op.
            if group_start > 0 {
                ops[group_start - 1].old_line.map(|l| l + 1).unwrap_or(0)
            } else {
                0
            }
        });

        // Collect old_lines and new_lines including context.
        let context_before_start = group_start.saturating_sub(context);
        let context_after_end = (group_end + context).min(ops.len());

        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();

        for k in context_before_start..context_after_end {
            match ops[k].tag {
                ChangeTag::Equal => {
                    old_lines.push(ops[k].value.clone());
                    new_lines.push(ops[k].value.clone());
                }
                ChangeTag::Delete => {
                    old_lines.push(ops[k].value.clone());
                }
                ChangeTag::Insert => {
                    new_lines.push(ops[k].value.clone());
                }
            }
        }

        // The old_start (1-indexed) is the old-line of the first context-before line + 1,
        // or if no context before, the first change's old_line + 1.
        let old_start_1 = if context_before_start < group_start {
            // There are context lines before; their old_line is the first one.
            ops[context_before_start].old_line.map(|l| l + 1).unwrap_or(1)
        } else {
            hunk_old_start_0 + 1
        };

        hunks.push(PatchHunk {
            old_start: old_start_1 as u32,
            old_lines,
            new_lines,
        });

        i = group_end;
    }

    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_first_changed_line_basic() {
        assert_eq!(find_first_changed_line("a\nb\nc", "a\nB\nc"), Some(2));
        assert_eq!(find_first_changed_line("a\nb", "a\nb"), None);
    }

    #[test]
    fn build_line_map_identical() {
        let map = build_line_map("a\nb\nc", "a\nb\nc");
        assert_eq!(map.get(&1), Some(&1));
        assert_eq!(map.get(&2), Some(&2));
        assert_eq!(map.get(&3), Some(&3));
    }

    #[test]
    fn build_line_map_insertion_before() {
        let map = build_line_map("a\nb\nc", "X\na\nb\nc");
        // a (old 1) → new 2, b (old 2) → new 3, c (old 3) → new 4
        assert_eq!(map.get(&1), Some(&2));
        assert_eq!(map.get(&2), Some(&3));
        assert_eq!(map.get(&3), Some(&4));
    }

    #[test]
    fn structured_patch_apply_replaces_line() {
        let old = "l1\nl2\nl3\nl4\nl5\n";
        let new = "l1\nl2\nL3\nl4\nl5\n";
        let target = "l1\nl2\nl3\nl4\nl5\n";
        let result = structured_patch_apply(old, &new, target, 3, 0);
        assert_eq!(result.as_deref(), Some("l1\nl2\nL3\nl4\nl5\n"));
    }

    #[test]
    fn structured_patch_apply_refuses_on_mismatch() {
        let old = "l1\nl2\nl3\n";
        let new = "l1\nl2\nL3\n";
        let target = "l1\nCHANGED\nl3\n";
        let result = structured_patch_apply(old, &new, target, 3, 0);
        // The hunk wants to remove "l3" at a position where target has "l3" but
        // the context "l2" doesn't match "CHANGED" — should refuse.
        assert_eq!(result, None);
    }
}
