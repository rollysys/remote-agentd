//! Patcher — port of packages/hashline/src/patcher.ts
//!
//! High-level patch orchestrator. Reads each section's target file, verifies
//! the snapshot tag (with recovery), applies edits, and writes the result.

use std::path::Path as StdPath;

use crate::apply::apply_edits;
use crate::block::{has_block_edit, resolve_block_edits, OnUnresolved, ResolveBlockEditsOptions};
use crate::diff_preview::make_diff_preview;
use crate::format::{compute_file_hash, format_hashline_header};
use crate::messages::{missing_snapshot_tag_message, HEADTAIL_DRIFT_WARNING};
use crate::recovery::{Recovery, RecoveryArgs, RecoveryResult};
use crate::snapshots::InMemorySnapshotStore;
use crate::types::{
    ApplyResult, BlockResolution, Cursor, Edit, FileOp, Patch, PatchSection,
};

/// Upper bound on unseen anchor lines whose actual content is surfaced inline.
const SEEN_LINE_REVEAL_CAP: usize = 40;
/// Per-revealed-line character cap.
const SEEN_LINE_REVEAL_MAX_COLUMNS: usize = 512;

/// Per-section result returned by `Patcher::apply`.
#[derive(Debug, Clone)]
pub struct PatchSectionResult {
    /// Section path (as authored).
    pub path: String,
    /// Filesystem-canonical key for this section.
    pub canonical_path: String,
    /// `"noop"`, `"delete"`, `"create"`, or `"update"`.
    pub op: SectionOp,
    /// Pre-edit text (LF-normalized, BOM-stripped).
    pub before: String,
    /// Post-edit text (LF-normalized).
    pub after: String,
    /// 4-hex content-hash tag for `after`.
    pub file_hash: String,
    /// Hashline section header (`[path#tag]`) of the post-edit content.
    pub header: String,
    /// 1-indexed first changed line, or `None` for noops.
    pub first_changed_line: Option<u32>,
    /// Warnings collected by the parser, applier, and recovery.
    pub warnings: Vec<String>,
    /// Destination path when this section includes `MV DEST`.
    pub move_dest: Option<String>,
    /// Resolved block spans (when block ops matched the tagged content).
    pub block_resolutions: Vec<BlockResolution>,
    /// Unified diff preview.
    pub diff: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionOp {
    Create,
    Update,
    Delete,
    Noop,
}

/// Result of applying a complete patch.
#[derive(Debug, Clone)]
pub struct PatcherApplyResult {
    pub sections: Vec<PatchSectionResult>,
}

/// A prepared section: parsed, validated, and applied in memory but not yet
/// written to disk.
pub struct PreparedSection {
    pub section: PatchSection,
    pub canonical_path: String,
    pub exists: bool,
    pub raw_content: String,
    pub normalized: String,
    pub apply_result: ApplyResult,
    pub parse_warnings: Vec<String>,
    pub file_op: Option<FileOp>,
}

impl PreparedSection {
    /// True when the apply produced no change and no file op.
    pub fn is_noop(&self) -> bool {
        self.file_op.is_none() && self.apply_result.text == self.normalized
    }
}

/// High-level patcher. Wires a snapshot store with the parsing + applying core.
pub struct Patcher {
    pub snapshots: InMemorySnapshotStore,
    pub block_resolver: Option<crate::types::BlockResolver>,
}

impl Patcher {
    pub fn new() -> Self {
        Self {
            snapshots: InMemorySnapshotStore::new(),
            block_resolver: None,
        }
    }

    pub fn with_block_resolver(mut self, resolver: crate::types::BlockResolver) -> Self {
        self.block_resolver = Some(resolver);
        self
    }

    /// Apply every section in `patch`. All sections are prepared in memory
    /// before any write hits the filesystem, so a multi-section batch is
    /// all-or-nothing.
    pub fn apply(&mut self, patch: &Patch) -> anyhow::Result<PatcherApplyResult> {
        // Single-section fast path.
        if patch.sections.len() == 1 {
            let prepared = self.prepare(&patch.sections[0])?;
            return Ok(PatcherApplyResult {
                sections: vec![self.commit(prepared)?],
            });
        }

        let mut prepared: Vec<PreparedSection> = Vec::new();
        for section in &patch.sections {
            prepared.push(self.prepare(section)?);
        }
        assert_unique_canonical_paths(&prepared)?;
        for entry in &prepared {
            if entry.is_noop() {
                anyhow::bail!("Edits to {} resulted in no changes being made.", entry.section.path);
            }
        }

        let mut results: Vec<PatchSectionResult> = Vec::new();
        for entry in prepared.into_iter() {
            let path_label = entry.section.path.clone();
            match self.commit(entry) {
                Ok(result) => results.push(result),
                Err(error) => {
                    let message = error.to_string();
                    anyhow::bail!("Failed to write {path_label}: {message}");
                }
            }
        }
        Ok(PatcherApplyResult { sections: results })
    }

    /// Run the preflight pass only: read, parse, validate, apply-in-memory.
    /// No writes hit the filesystem.
    pub fn preflight(&mut self, patch: &Patch) -> anyhow::Result<()> {
        let mut prepared: Vec<PreparedSection> = Vec::new();
        for section in &patch.sections {
            prepared.push(self.prepare(section)?);
        }
        assert_unique_canonical_paths(&prepared)?;
        for entry in &prepared {
            if entry.is_noop() {
                anyhow::bail!("Edits to {} resulted in no changes being made.", entry.section.path);
            }
        }
        Ok(())
    }

    /// Read a section's target file, parse the section, validate the snapshot
    /// tag (with recovery), and apply the edits in memory.
    pub fn prepare(&mut self, section: &PatchSection) -> anyhow::Result<PreparedSection> {
        let parse_warnings = section.warnings.clone();
        let file_op = section.file_op.clone();
        assert_section_hash_present(&section.path, section.file_hash.as_deref())?;

        let canonical_path = canonicalize(&section.path);
        let (exists, raw_content) = try_read(&section.path);

        // Path recovery: the authored path doesn't exist, but its filename +
        // snapshot tag may name a file read this session.
        let (target_path, target_canonical, target_exists, target_raw) = if !exists {
            if let Some(recovered) = self.recover_section_path_from_tag(section, &canonical_path) {
                let (e, r) = try_read(&recovered.0);
                (recovered.0, recovered.1, e, r)
            } else {
                (section.path.clone(), canonical_path, exists, raw_content)
            }
        } else {
            (section.path.clone(), canonical_path, exists, raw_content)
        };

        if !target_exists {
            if let Some(FileOp::Mv { dest }) = &file_op {
                if canonicalize(dest) == target_canonical {
                    anyhow::bail!("MV destination is the same as {}.", target_path);
                }
            }
            anyhow::bail!("File not found: {}. Use the write tool to create new files.", target_path);
        }

        if let Some(FileOp::Mv { dest }) = &file_op {
            if canonicalize(dest) == target_canonical {
                anyhow::bail!("MV destination is the same as {}.", target_path);
            }
        }

        let normalized = strip_bom(&target_raw);

        let edits = if file_op == Some(FileOp::Rem) {
            Vec::new()
        } else {
            section.edits.clone()
        };

        let apply_result = self.apply_with_recovery(
            &target_path,
            &target_canonical,
            target_exists,
            &normalized,
            &edits,
            section,
        )?;

        Ok(PreparedSection {
            section: PatchSection {
                path: target_path,
                ..section.clone()
            },
            canonical_path: target_canonical,
            exists: target_exists,
            raw_content: target_raw,
            normalized,
            apply_result,
            parse_warnings,
            file_op,
        })
    }

    /// Commit a prepared section to the filesystem.
    pub fn commit(&mut self, prepared: PreparedSection) -> anyhow::Result<PatchSectionResult> {
        let PreparedSection {
            section,
            canonical_path,
            exists,
            normalized,
            apply_result,
            parse_warnings,
            file_op,
            ..
        } = prepared;

        let after = apply_result.text;
        let mut warnings = parse_warnings;
        warnings.extend(apply_result.warnings.iter().cloned());
        let move_dest = file_op.as_ref().and_then(|op| match op {
            FileOp::Mv { dest } => Some(dest.clone()),
            _ => None,
        });

        // REM: delete the file.
        if file_op == Some(FileOp::Rem) {
            std::fs::remove_file(&section.path).ok();
            self.snapshots.invalidate(&canonical_path);
            let hash = compute_file_hash(&normalized);
            let header = format_hashline_header(&section.path, &hash);
            return Ok(PatchSectionResult {
                path: section.path,
                canonical_path,
                op: SectionOp::Delete,
                before: normalized.clone(),
                after: normalized,
                file_hash: hash,
                header,
                first_changed_line: None,
                warnings,
                move_dest: None,
                block_resolutions: Vec::new(),
                diff: String::new(),
            });
        }

        // No-op: edits produced no change.
        if after == normalized && move_dest.is_none() {
            let hash = self.record_full_snapshot(&canonical_path, &normalized);
            let header = format_hashline_header(&section.path, &hash);
            return Ok(PatchSectionResult {
                path: section.path,
                canonical_path,
                op: SectionOp::Noop,
                before: normalized.clone(),
                after: normalized,
                file_hash: hash,
                header,
                first_changed_line: None,
                warnings,
                move_dest: None,
                block_resolutions: Vec::new(),
                diff: String::new(),
            });
        }

        // MV: move the file.
        if let Some(dest) = &move_dest {
            let dest_canonical = canonicalize(dest);
            self.snapshots.relocate(&canonical_path, &dest_canonical);
            std::fs::write(dest, &after)?;
            std::fs::remove_file(&section.path).ok();
            let file_hash = self.record_full_snapshot(&dest_canonical, &after);
            let diff = make_diff_preview(&normalized, &after);
            let header = format_hashline_header(dest, &file_hash);
            return Ok(PatchSectionResult {
                path: dest.clone(),
                canonical_path: dest_canonical,
                op: SectionOp::Update,
                before: normalized,
                after,
                file_hash,
                header,
                first_changed_line: apply_result.first_changed_line,
                warnings,
                move_dest: Some(dest.clone()),
                block_resolutions: apply_result.block_resolutions,
                diff,
            });
        }

        // Normal write.
        std::fs::write(&section.path, &after)?;
        let file_hash = self.record_full_snapshot(&canonical_path, &after);
        let op = if exists { SectionOp::Update } else { SectionOp::Create };
        let diff = make_diff_preview(&normalized, &after);
        let header = format_hashline_header(&section.path, &file_hash);

        Ok(PatchSectionResult {
            path: section.path,
            canonical_path,
            op,
            before: normalized,
            after,
            file_hash,
            header,
            first_changed_line: apply_result.first_changed_line,
            warnings,
            move_dest: None,
            block_resolutions: apply_result.block_resolutions,
            diff,
        })
    }

    // ── Internal helpers ───────────────────────────────────────────────

    fn record_full_snapshot(&mut self, canonical_path: &str, normalized: &str) -> String {
        self.snapshots.record(canonical_path, normalized)
    }

    /// Resolve a missing authored path to a file read this session by matching
    /// its filename and snapshot tag.
    fn recover_section_path_from_tag(
        &self,
        section: &PatchSection,
        original_canonical: &str,
    ) -> Option<(String, String)> {
        let tag = section.file_hash.as_ref()?;
        let authored_name = basename(&section.path);
        let mut candidates: Vec<String> = Vec::new();
        for candidate in self.snapshots.find_by_hash(tag) {
            if basename(candidate) == authored_name && canonicalize(candidate) != original_canonical {
                candidates.push(candidate.to_string());
            }
        }
        if candidates.len() != 1 {
            return None;
        }
        let resolved = candidates.into_iter().next()?;
        Some((resolved.clone(), canonicalize(&resolved)))
    }

    fn apply_with_recovery(
        &mut self,
        section_path: &str,
        canonical_path: &str,
        exists: bool,
        normalized: &str,
        edits: &[Edit],
        section: &PatchSection,
    ) -> anyhow::Result<ApplyResult> {
        let expected = if exists { section.file_hash.as_deref() } else { None };
        let live_matches = expected.map_or(false, |tag| compute_file_hash(normalized) == tag);

        // Extract matched_snapshot data upfront to avoid borrow conflicts.
        let matched_snapshot_data: Option<(String, String, Vec<u32>)> = if live_matches {
            self.snapshots.by_content(canonical_path, normalized).map(|s| {
                (s.content.clone(), s.tag.clone(), s.seen_lines.clone())
            })
        } else {
            None
        };

        // Resolve block edits before recovery.
        let mut block_resolutions: Vec<BlockResolution> = Vec::new();
        let mut resolve_warnings: Vec<String> = Vec::new();
        let mut resolved: Vec<Edit> = edits.to_vec();

        if has_block_edit(edits) {
            let stored_snap = expected.and_then(|tag| self.snapshots.by_hash(canonical_path, tag));
            let base_text = if expected.is_none() || live_matches {
                normalized.to_string()
            } else if let Some(snap) = &stored_snap {
                snap.content.clone()
            } else {
                let actual_hash = self.record_full_snapshot(canonical_path, normalized);
                anyhow::bail!(
                    "{}",
                    mismatch_error_message(section_path, expected.unwrap_or(""), &actual_hash, false)
                );
            };

            let mut on_resolved = |res: &BlockResolution| block_resolutions.push(res.clone());
            let mut on_warning = |w: &str| resolve_warnings.push(w.to_string());
            let mut opts = ResolveBlockEditsOptions {
                on_unresolved: OnUnresolved::Throw,
                on_resolved: Some(&mut on_resolved),
                on_warning: Some(&mut on_warning),
            };
            resolved = resolve_block_edits(
                resolved,
                &base_text,
                section_path,
                self.block_resolver.as_ref(),
                &mut opts,
            )?;
        }

        let with_resolve_warnings = |mut result: ApplyResult| {
            if !resolve_warnings.is_empty() {
                let mut w = resolve_warnings.clone();
                w.extend(result.warnings);
                result.warnings = w;
            }
            result
        };

        // No tag, or the tag still names the live content: apply directly.
        if expected.is_none() || live_matches {
            // Seen-line check: reject edits anchored on unread lines.
            if let Some(tag) = expected {
                if let Some((content, snap_tag, seen_lines)) = &matched_snapshot_data {
                    self.assert_seen_lines(section, tag, snap_tag, seen_lines, content)?;
                }
            }
            let mut result = apply_edits(normalized, &resolved);
            if !block_resolutions.is_empty() {
                result.block_resolutions = block_resolutions.clone();
            }
            return Ok(with_resolve_warnings(result));
        }

        // Head/tail-only inserts are position-stable: apply with a drift warning.
        if !has_anchor_scoped_edit(&resolved) {
            let mut result = apply_edits(normalized, &resolved);
            let mut w = vec![HEADTAIL_DRIFT_WARNING.to_string()];
            w.extend(result.warnings);
            result.warnings = w;
            return Ok(with_resolve_warnings(result));
        }

        // File drifted: try recovery.
        let recovery = Recovery::new(&self.snapshots);
        let recovered = recovery.try_recover(RecoveryArgs {
            path: canonical_path,
            current_text: normalized,
            file_hash: expected.unwrap(),
            edits: &resolved,
        });
        if let Some(recovered) = recovered {
            return Ok(with_resolve_warnings(recovery_to_apply_result(recovered)));
        }

        // Recovery failed: mismatch error.
        let hash_recognized = self
            .snapshots
            .by_hash(canonical_path, expected.unwrap())
            .is_some();
        let actual_hash = self.record_full_snapshot(canonical_path, normalized);
        anyhow::bail!(
            "{}",
            mismatch_error_message(section_path, expected.unwrap(), &actual_hash, hash_recognized)
        );
    }

    /// Reject an anchored edit referencing a line the read never displayed.
    fn assert_seen_lines(
        &mut self,
        section: &PatchSection,
        tag: &str,
        snap_tag: &str,
        seen_lines: &[u32],
        content: &str,
    ) -> anyhow::Result<()> {
        if seen_lines.is_empty() {
            return Ok(());
        }
        let anchor_lines = collect_section_anchor_lines(section);
        let unseen: Vec<u32> = anchor_lines
            .iter()
            .copied()
            .filter(|l| !seen_lines.contains(l))
            .collect();
        if unseen.is_empty() {
            return Ok(());
        }
        let source_lines: Vec<&str> = content.split('\n').collect();
        let mut revealed: Vec<crate::messages::RevealedLine> = Vec::new();
        let reveal_count = unseen.len().min(SEEN_LINE_REVEAL_CAP);
        let mut column_truncated = false;
        for &line in unseen.iter().take(reveal_count) {
            if line < 1 || (line as usize) > source_lines.len() {
                continue;
            }
            let source = source_lines[(line - 1) as usize];
            if source.len() > SEEN_LINE_REVEAL_MAX_COLUMNS {
                revealed.push(crate::messages::RevealedLine {
                    line,
                    text: format!("{}…", &source[..SEEN_LINE_REVEAL_MAX_COLUMNS]),
                });
                column_truncated = true;
            } else {
                revealed.push(crate::messages::RevealedLine {
                    line,
                    text: source.to_string(),
                });
            }
        }
        let truncated = unseen.len() > revealed.len() || column_truncated;

        // Only merge when the reveal covered every unseen line in full.
        if !truncated {
            let lines_to_merge: Vec<u32> = revealed.iter().map(|r| r.line).collect();
            self.snapshots
                .record_seen_lines(&section.path, snap_tag, lines_to_merge);
        }

        let reveal = crate::messages::UnseenLinesReveal {
            lines: revealed,
            truncated,
        };
        anyhow::bail!(
            "{}",
            crate::messages::unseen_lines_message(&section.path, &unseen, tag, &reveal)
        );
    }
}

impl Default for Patcher {
    fn default() -> Self {
        Self::new()
    }
}

// ── Free functions ──────────────────────────────────────────────────────

fn assert_section_hash_present(section_path: &str, file_hash: Option<&str>) -> anyhow::Result<()> {
    if file_hash.is_some() {
        return Ok(());
    }
    anyhow::bail!("{}", missing_snapshot_tag_message(section_path));
}

fn assert_unique_canonical_paths(prepared: &[PreparedSection]) -> anyhow::Result<()> {
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for entry in prepared {
        if let Some(previous) = seen.get(&entry.canonical_path) {
            anyhow::bail!(
                "Multiple hashline sections resolve to the same file ({} and {}). Merge their ops under one header before applying.",
                previous,
                entry.section.path
            );
        }
        seen.insert(entry.canonical_path.clone(), entry.section.path.clone());
    }
    Ok(())
}

fn recovery_to_apply_result(result: RecoveryResult) -> ApplyResult {
    ApplyResult {
        text: result.text,
        first_changed_line: result.first_changed_line,
        warnings: result.warnings,
        block_resolutions: Vec::new(),
    }
}

fn has_anchor_scoped_edit(edits: &[Edit]) -> bool {
    edits.iter().any(|edit| match edit {
        Edit::Delete { .. } => true,
        Edit::Block { .. } => true,
        Edit::Insert { cursor, .. } => {
            matches!(cursor, Cursor::BeforeAnchor(_) | Cursor::AfterAnchor(_))
        }
    })
}

/// Collect anchor lines from a section's edits.
fn collect_section_anchor_lines(section: &PatchSection) -> Vec<u32> {
    let mut lines = Vec::new();
    for edit in &section.edits {
        match edit {
            Edit::Delete { anchor } => lines.push(anchor.line),
            Edit::Block { anchor, .. } => lines.push(anchor.line),
            Edit::Insert { cursor, .. } => match cursor {
                Cursor::BeforeAnchor(a) | Cursor::AfterAnchor(a) => lines.push(a.line),
                Cursor::Bof | Cursor::Eof => {}
            },
        }
    }
    lines
}

/// Format a mismatch error message (ported from MismatchError.formatMessage).
fn mismatch_error_message(
    path: &str,
    expected: &str,
    actual: &str,
    hash_recognized: bool,
) -> String {
    use crate::format::{HL_FILE_HASH_SEP, HL_FILE_PREFIX, HL_FILE_SUFFIX};
    let path_text = if path.is_empty() {
        String::new()
    } else {
        format!(" for {path}")
    };
    if !hash_recognized {
        format!(
            "Edit rejected{path_text}: hash {HL_FILE_HASH_SEP}{expected} is not from this session.\nThe current file hashes to {HL_FILE_HASH_SEP}{actual}. Re-read the file with `read` to copy a current {HL_FILE_PREFIX}path{HL_FILE_HASH_SEP}tag{HL_FILE_SUFFIX} header — never invent the tag and never reuse one from a prior session."
        )
    } else {
        format!(
            "Edit rejected{path_text}: file changed between read and edit.\nSection is bound to {HL_FILE_HASH_SEP}{expected}, but the current file hashes to {HL_FILE_HASH_SEP}{actual}. If a prior edit in this session modified this file, copy the {HL_FILE_PREFIX}path{HL_FILE_HASH_SEP}newhash{HL_FILE_SUFFIX} header from that edit's response; otherwise re-read the file with `read` to refresh the tag before retrying."
        )
    }
}

/// Strip a UTF-8 BOM if present.
fn strip_bom(content: &str) -> String {
    if let Some(stripped) = content.strip_prefix('\u{FEFF}') {
        stripped.to_string()
    } else {
        content.to_string()
    }
}

/// Canonicalize a path for uniqueness checks.
fn canonicalize(path: &str) -> String {
    StdPath::new(path)
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// Get the basename (last path component) of a path.
fn basename(path: &str) -> &str {
    StdPath::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Try to read a file. Returns (exists, content).
fn try_read(path: &str) -> (bool, String) {
    match std::fs::read_to_string(path) {
        Ok(content) => (true, content),
        Err(_) => (false, String::new()),
    }
}
