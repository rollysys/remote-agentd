//! Snapshot store — port of packages/hashline/src/snapshots.ts
//!
//! Maintains file content hashes for hashline edit verification.

use std::collections::{HashMap, VecDeque};

use crate::format::compute_file_hash;

const MAX_PATHS: usize = 30;
const MAX_VERSIONS_PER_PATH: usize = 4;

/// A snapshot of a file's content at a point in time.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub tag: String,
    pub content: String,
    /// Line numbers that were visible to the agent (for read provenance).
    pub seen_lines: Vec<u32>,
}

/// In-memory snapshot store with LRU eviction.
#[derive(Debug, Default)]
pub struct InMemorySnapshotStore {
    /// path → version history (most recent first)
    store: HashMap<String, VecDeque<Snapshot>>,
    /// LRU order of paths (most recently used at back)
    lru: VecDeque<String>,
}

impl InMemorySnapshotStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record file content and return its tag.
    pub fn record(&mut self, path: &str, content: &str) -> String {
        let tag = compute_file_hash(content);

        // Check if path already exists
        if !self.store.contains_key(path) {
            // Evict if over capacity
            while self.store.len() >= MAX_PATHS {
                if let Some(old_path) = self.lru.pop_front() {
                    self.store.remove(&old_path);
                } else {
                    break;
                }
            }
            self.store.insert(path.to_string(), VecDeque::new());
        }

        let versions = self.store.get_mut(path).unwrap();

        // Check if this tag already exists (fuse identical content)
        if let Some(_existing) = versions.iter().find(|s| s.tag == tag) {
            // Promote to front
            let pos = versions.iter().position(|s| s.tag == tag).unwrap();
            let existing = versions.remove(pos).unwrap();
            versions.push_front(Snapshot {
                tag: existing.tag,
                content: content.to_string(),
                seen_lines: existing.seen_lines.clone(),
            });
            return tag;
        }

        // Add new version at front
        versions.push_front(Snapshot {
            tag: tag.clone(),
            content: content.to_string(),
            seen_lines: Vec::new(),
        });

        // Trim to max versions
        while versions.len() > MAX_VERSIONS_PER_PATH {
            versions.pop_back();
        }

        // Update LRU
        self.lru.retain(|p| p != path);
        self.lru.push_back(path.to_string());

        tag
    }

    /// Check if a tag is a known snapshot for this path.
    pub fn verify(&self, path: &str, tag: &str) -> bool {
        self.store
            .get(path)
            .map(|versions| versions.iter().any(|s| s.tag == tag))
            .unwrap_or(false)
    }

    /// Get the most recent tag for a path.
    pub fn current_tag(&self, path: &str) -> Option<&str> {
        self.store
            .get(path)
            .and_then(|v| v.front())
            .map(|s| s.tag.as_str())
    }

    /// Look up a snapshot by tag.
    pub fn by_tag(&self, path: &str, tag: &str) -> Option<&Snapshot> {
        self.store
            .get(path)
            .and_then(|v| v.iter().find(|s| s.tag == tag))
    }

    /// Find all paths that have a version with the given tag.
    pub fn find_by_tag(&self, tag: &str) -> Vec<&str> {
        self.store
            .iter()
            .filter(|(_, versions)| versions.iter().any(|s| s.tag == tag))
            .map(|(path, _)| path.as_str())
            .collect()
    }

    /// Invalidate all snapshots for a path.
    pub fn invalidate(&mut self, path: &str) {
        self.store.remove(path);
        self.lru.retain(|p| p != path);
    }

    /// Clear all snapshots.
    pub fn clear(&mut self) {
        self.store.clear();
        self.lru.clear();
    }

    /// Look up a snapshot by hash tag (alias for by_tag).
    pub fn by_hash(&self, path: &str, tag: &str) -> Option<&Snapshot> {
        self.by_tag(path, tag)
    }

    /// Find all paths that have a version with the given hash tag (alias for find_by_tag).
    pub fn find_by_hash(&self, tag: &str) -> Vec<&str> {
        self.find_by_tag(tag)
    }

    /// Get the most recent (head) snapshot for a path.
    pub fn head(&self, path: &str) -> Option<&Snapshot> {
        self.store.get(path).and_then(|v| v.front())
    }

    /// Look up a snapshot by content hash.
    pub fn by_content(&self, path: &str, content: &str) -> Option<&Snapshot> {
        let tag = compute_file_hash(content);
        self.by_tag(path, &tag)
    }

    /// Record which lines were visible to the agent for a given snapshot.
    pub fn record_seen_lines(&mut self, path: &str, tag: &str, lines: Vec<u32>) {
        if let Some(versions) = self.store.get_mut(path) {
            if let Some(snap) = versions.iter_mut().find(|s| s.tag == tag) {
                snap.seen_lines = lines;
            }
        }
    }

    /// Get the seen lines for a snapshot.
    pub fn get_seen_lines(&self, path: &str, tag: &str) -> Option<&Vec<u32>> {
        self.by_tag(path, tag).map(|s| &s.seen_lines)
    }

    /// Move version history and read provenance from one path to another.
    pub fn relocate(&mut self, old_path: &str, new_path: &str) {
        if let Some(versions) = self.store.remove(old_path) {
            self.store.insert(new_path.to_string(), versions);
        }
        self.lru.iter_mut().for_each(|p| {
            if p == old_path {
                *p = new_path.to_string();
            }
        });
    }
}

/// Trait for snapshot stores (allows alternative implementations).
pub trait SnapshotStoreTrait: Send + Sync {
    fn record(&mut self, path: &str, content: &str) -> String;
    fn verify(&self, path: &str, tag: &str) -> bool;
    fn current_tag(&self, path: &str) -> Option<String>;
}

impl SnapshotStoreTrait for InMemorySnapshotStore {
    fn record(&mut self, path: &str, content: &str) -> String {
        self.record(path, content)
    }

    fn verify(&self, path: &str, tag: &str) -> bool {
        self.verify(path, tag)
    }

    fn current_tag(&self, path: &str) -> Option<String> {
        self.current_tag(path).map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "/tmp/test.ts";
    const TAG_RE: &str = r"^[0-9A-F]{4}$";

    #[test]
    fn test_derives_tag_from_content() {
        let mut store = InMemorySnapshotStore::new();
        let tag = store.record(PATH, "L1\nL2\nL3\n");
        assert!(regex::Regex::new(TAG_RE).unwrap().is_match(&tag));
        assert_eq!(tag, compute_file_hash("L1\nL2\nL3\n"));
    }

    #[test]
    fn test_fuses_identical_content() {
        let mut store = InMemorySnapshotStore::new();
        let tag1 = store.record(PATH, "hello\n");
        let tag2 = store.record(PATH, "hello\n");
        assert_eq!(tag1, tag2);
    }

    #[test]
    fn test_new_tag_on_content_change() {
        let mut store = InMemorySnapshotStore::new();
        let tag1 = store.record(PATH, "before\n");
        let tag2 = store.record(PATH, "after\n");
        assert_ne!(tag1, tag2);
    }

    #[test]
    fn test_verify() {
        let mut store = InMemorySnapshotStore::new();
        let tag = store.record(PATH, "hello\n");
        assert!(store.verify(PATH, &tag));
        assert!(!store.verify(PATH, "0000"));
    }

    #[test]
    fn test_cross_path_rejected() {
        let mut store = InMemorySnapshotStore::new();
        let tag = store.record(PATH, "hello\n");
        assert!(!store.verify("/tmp/other.ts", &tag));
    }

    #[test]
    fn test_invalidate() {
        let mut store = InMemorySnapshotStore::new();
        let tag = store.record(PATH, "hello\n");
        assert!(store.verify(PATH, &tag));
        store.invalidate(PATH);
        assert!(!store.verify(PATH, &tag));
    }
}
