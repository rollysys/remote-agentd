//! Hashline type definitions — port of packages/hashline/src/types.ts

use std::fmt;

/// A 1-indexed line anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Anchor {
    pub line: u32,
}

/// Cursor position for insertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    /// Before a specific anchor line.
    BeforeAnchor(Anchor),
    /// After a specific anchor line.
    AfterAnchor(Anchor),
    /// Beginning of file.
    Bof,
    /// End of file.
    Eof,
}

/// An edit operation parsed from a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Insert text at a cursor position.
    Insert {
        cursor: Cursor,
        text: String,
        mode: InsertMode,
    },
    /// Delete a range of lines.
    Delete {
        anchor: Anchor,
    },
    /// Block edit (SWAP.BLK / DEL.BLK / INS.BLK.POST) — needs tree-sitter resolution.
    Block {
        anchor: Anchor,
        payloads: Vec<String>,
        mode: BlockMode,
    },
}

/// Block edit mode — distinguishes SWAP.BLK, DEL.BLK, and INS.BLK.POST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMode {
    /// SWAP.BLK N: — replace block with payloads.
    Replace,
    /// DEL.BLK N — delete block.
    Delete,
    /// INS.BLK.POST N: — insert after block end.
    InsertAfter,
}

/// Insert mode distinguishes replacement inserts from pure inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertMode {
    /// A pure insertion (INS.PRE/POST/HEAD/TAIL).
    Insert,
    /// A replacement insert (part of SWAP).
    Replacement,
}

/// A parsed range like `5.=10`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedRange {
    pub start: u32,
    pub end: u32,
}

/// Result of applying edits to text.
#[derive(Debug, Clone)]
pub struct ApplyResult {
    pub text: String,
    pub warnings: Vec<String>,
    pub first_changed_line: Option<u32>,
    pub block_resolutions: Vec<BlockResolution>,
}

/// A block resolution result — reported back to the model so it can confirm
/// which syntactic construct tree-sitter resolved for a SWAP.BLK/DEL.BLK op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockResolution {
    pub anchor_line: u32,
    pub start: u32,
    pub end: u32,
    pub op: BlockOp,
}

/// Block operation type for resolution results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOp {
    Replace,
    Delete,
    InsertAfter,
}

/// A parsed patch section: `[path#tag]` header + diff body.
#[derive(Debug, Clone)]
pub struct PatchSection {
    pub path: String,
    pub file_hash: Option<String>,
    pub edits: Vec<Edit>,
    pub file_op: Option<FileOp>,
    pub warnings: Vec<String>,
}

/// File-level operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    /// Remove the file (REM keyword).
    Rem,
    /// Move/rename the file (MV keyword).
    Mv { dest: String },
}

/// A complete patch with one or more sections.
#[derive(Debug, Clone)]
pub struct Patch {
    pub sections: Vec<PatchSection>,
}

/// Options for splitting hashline input.
#[derive(Debug, Clone, Default)]
pub struct SplitOptions {
    pub cwd: Option<String>,
    pub path: Option<String>,
}

/// A block span resolved by a tree-sitter BlockResolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSpan {
    pub start: u32,
    pub end: u32,
}

/// Request for block resolution.
#[derive(Debug, Clone)]
pub struct BlockResolverRequest {
    pub code: String,
    pub lang: Option<String>,
    pub path: Option<String>,
    pub line: u32,
}

/// A function that resolves a block range for SWAP.BLK/DEL.BLK operations.
pub type BlockResolver = Box<dyn Fn(&BlockResolverRequest) -> Option<BlockSpan> + Send + Sync>;

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cursor::BeforeAnchor(a) => write!(f, "before line {}", a.line),
            Cursor::AfterAnchor(a) => write!(f, "after line {}", a.line),
            Cursor::Bof => write!(f, "beginning of file"),
            Cursor::Eof => write!(f, "end of file"),
        }
    }
}

impl Anchor {
    pub fn new(line: u32) -> Self {
        Self { line }
    }
}
