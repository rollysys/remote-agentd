//! Hashline patch engine — Rust port of @oh-my-pi/hashline.
//!
//! Implements the patch language (SWAP/DEL/INS), snapshot store,
//! file hashing, and edit application with boundary repair.

pub mod format;
pub mod types;
pub mod tokenizer;
pub mod parser;
pub mod apply;
pub mod snapshots;
pub mod patcher;
pub mod prefixes;
pub mod messages;
pub mod block;
pub mod recovery;
pub mod diff_preview;

// Re-export the most used types and functions
pub use format::{compute_file_hash, format_hashline_header, format_numbered_line};
pub use types::*;
pub use parser::{parse_patch, ParseResult};
pub use apply::apply_edits;
pub use snapshots::{InMemorySnapshotStore, SnapshotStoreTrait};
pub use patcher::Patcher;
