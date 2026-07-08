//! Block resolution — port of packages/hashline/src/block.ts
//!
//! Expand deferred block edits (`SWAP.BLK N:` / `DEL.BLK N` /
//! `INS.BLK.POST N:`) into concrete inserts + deletes.

use crate::messages::{
    block_single_line_message, block_unresolved_message, insert_after_block_closer_lowered_warning,
    insert_after_block_unresolved_lowered_warning, BLOCK_RESOLVER_UNAVAILABLE,
};
use crate::types::{
    Anchor, BlockMode, BlockOp, BlockResolution, BlockResolver, BlockResolverRequest, BlockSpan,
    Cursor, Edit, InsertMode,
};


/// How to handle a replace/delete block edit that cannot be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnUnresolved {
    /// Raise an error (authoritative apply + final preview paths). Default.
    #[default]
    Throw,
    /// Silently skip the edit (streaming preview, transient parse errors).
    Drop,
}

/// Options for `resolve_block_edits`.
#[derive(Default)]
pub struct ResolveBlockEditsOptions<'a> {
    /// How to handle an unresolvable replace/delete block edit.
    pub on_unresolved: OnUnresolved,
    /// Callback invoked per successfully resolved block edit.
    pub on_resolved: Option<&'a mut dyn FnMut(&BlockResolution)>,
    /// Callback invoked per diagnostic warning.
    pub on_warning: Option<&'a mut dyn FnMut(&str)>,
}

/// True when at least one edit is an unresolved deferred block edit.
pub fn has_block_edit(edits: &[Edit]) -> bool {
    edits.iter().any(|e| matches!(e, Edit::Block { .. }))
}

/// Regex matching a line that is nothing but closing delimiters: `}`, `)`, `];`, `})`, `},`.
/// Ported from `STRUCTURAL_CLOSER_RE` in apply.ts.
fn is_structural_closer(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Every non-whitespace char must be ) ] } ; or ,
    trimmed.chars().all(|c| matches!(c, ')' | ']' | '}' | ';' | ','))
}

/// Convert `BlockMode` to `BlockOp` for diagnostics.
fn mode_to_op(mode: BlockMode) -> BlockOp {
    match mode {
        BlockMode::Replace => BlockOp::Replace,
        BlockMode::Delete => BlockOp::Delete,
        BlockMode::InsertAfter => BlockOp::InsertAfter,
    }
}

/// Resolve every deferred block edit in `edits` against `text` (parsed as the
/// language inferred from `path`). Non-block edits pass through untouched.
/// Returns a fresh edit list with no `Block` variants.
///
/// Synthesized inserts/deletes mirror the parser's concrete-form expansion:
/// - `SWAP.BLK N:` → one `before_anchor` replacement insert per payload at
///   `span.start`, then one delete per line across `[span.start, span.end]`.
/// - `DEL.BLK N` → one delete per line across `[span.start, span.end]`.
/// - `INS.BLK.POST N:` → one `after_anchor` insert per payload at `span.end`.
///
/// Returns `Err` when `on_unresolved == Throw` and a block edit cannot resolve.
pub fn resolve_block_edits(
    edits: Vec<Edit>,
    text: &str,
    path: &str,
    resolver: Option<&BlockResolver>,
    options: &mut ResolveBlockEditsOptions<'_>,
) -> anyhow::Result<Vec<Edit>> {
    if !has_block_edit(&edits) {
        return Ok(edits);
    }
    let on_unresolved = options.on_unresolved;
    let mut resolved: Vec<Edit> = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();

    for edit in edits {
        match &edit {
            Edit::Block {
                anchor,
                payloads,
                mode,
            } => {
                let op = mode_to_op(*mode);
                let span = resolver.and_then(|r| {
                    r(&BlockResolverRequest {
                        code: text.to_string(),
                        lang: None,
                        path: Some(path.to_string()),
                        line: anchor.line,
                    })
                });

                match span {
                    None => {
                        // `INS.BLK.POST N:` never fails — lower to plain `INS.POST N:`.
                        if *mode == BlockMode::InsertAfter {
                            let anchor_text = lines.get((anchor.line - 1) as usize).copied();
                            let is_closer = anchor_text.map_or(false, is_structural_closer);
                            if let Some(cb) = &mut options.on_warning {
                                cb(if is_closer {
                                    insert_after_block_closer_lowered_warning(anchor.line)
                                } else {
                                    insert_after_block_unresolved_lowered_warning(anchor.line)
                                }
                                .as_str());
                            }
                            for payload in payloads {
                                resolved.push(Edit::Insert {
                                    cursor: Cursor::AfterAnchor(*anchor),
                                    text: payload.clone(),
                                    mode: InsertMode::Insert,
                                });
                            }
                            continue;
                        }
                        // replace/delete: throw or drop
                        match on_unresolved {
                            OnUnresolved::Drop => continue,
                            OnUnresolved::Throw => {
                                let msg = if resolver.is_some() {
                                    block_unresolved_message(anchor.line, op, Some(&lines.iter().map(|s| s.to_string()).collect::<Vec<_>>()))
                                } else {
                                    BLOCK_RESOLVER_UNAVAILABLE.to_string()
                                };
                                anyhow::bail!("line {}: {}", anchor.line, msg);
                            }
                        }
                    }
                    Some(span) => {
                        let span: BlockSpan = span;
                        if span.start == span.end {
                            // Single-line block → mis-anchor; reject or drop.
                            match on_unresolved {
                                OnUnresolved::Drop => continue,
                                OnUnresolved::Throw => {
                                    anyhow::bail!("line {}: {}", anchor.line, block_single_line_message(anchor.line, op));
                                }
                            }
                        }

                        // Report the resolution.
                        let resolution = BlockResolution {
                            anchor_line: anchor.line,
                            start: span.start,
                            end: span.end,
                            op,
                        };
                        if let Some(cb) = &mut options.on_resolved {
                            cb(&resolution);
                        }

                        if *mode == BlockMode::InsertAfter {
                            // One after_anchor insert per payload at span.end.
                            for payload in payloads {
                                resolved.push(Edit::Insert {
                                    cursor: Cursor::AfterAnchor(Anchor::new(span.end)),
                                    text: payload.clone(),
                                    mode: InsertMode::Insert,
                                });
                            }
                            continue;
                        }

                        // Replace / Delete: replacement inserts at span.start, then deletes.
                        for payload in payloads {
                            resolved.push(Edit::Insert {
                                cursor: Cursor::BeforeAnchor(Anchor::new(span.start)),
                                text: payload.clone(),
                                mode: InsertMode::Replacement,
                            });
                        }
                        for line in span.start..=span.end {
                            resolved.push(Edit::Delete {
                                anchor: Anchor::new(line),
                            });
                        }
                    }
                }
            }
            // Non-block edits pass through untouched.
            _ => resolved.push(edit),
        }
    }
    Ok(resolved)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_no_block_edits() {
        let edits = vec![Edit::Insert {
            cursor: Cursor::Eof,
            text: "x".into(),
            mode: InsertMode::Insert,
        }];
        let mut opts = ResolveBlockEditsOptions::default();
        let out = resolve_block_edits(edits.clone(), "", "f.ts", None, &mut opts).unwrap();
        assert_eq!(out, edits);
    }

    #[test]
    fn delete_block_without_resolver_throws() {
        let edits = vec![Edit::Block {
            anchor: Anchor::new(2),
            payloads: vec![],
            mode: BlockMode::Delete,
        }];
        let mut opts = ResolveBlockEditsOptions {
            on_unresolved: OnUnresolved::Throw,
            ..Default::default()
        };
        let result = resolve_block_edits(edits, "a\nb\nc\n", "f.ts", None, &mut opts);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("block resolver"));
    }

    #[test]
    fn insert_after_block_without_resolver_lowers_to_insert_post() {
        let edits = vec![Edit::Block {
            anchor: Anchor::new(2),
            payloads: vec!["NEW".into()],
            mode: BlockMode::InsertAfter,
        }];
        let mut warning: Option<String> = None;
        let mut opts = ResolveBlockEditsOptions {
            on_unresolved: OnUnresolved::Throw,
            on_resolved: None,
            on_warning: Some(&mut |w: &str| warning = Some(w.to_string())),
        };
        let out = resolve_block_edits(edits, "a\nb\nc\n", "f.ts", None, &mut opts).unwrap();
        assert_eq!(out.len(), 1);
        match &out[0] {
            Edit::Insert {
                cursor,
                text,
                mode,
            } => {
                assert_eq!(*cursor, Cursor::AfterAnchor(Anchor::new(2)));
                assert_eq!(text, "NEW");
                assert_eq!(*mode, InsertMode::Insert);
            }
            _ => panic!("expected Insert"),
        }
        assert!(warning.is_some());
    }
}
