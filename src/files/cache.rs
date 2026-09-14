//! LRU file state cache — Rust port of `src/files/cache.ts`.
//!
//! Limits: 100 entries / 25 MB total content. `get` touches (moves to MRU).
//! `clone` deep-copies (sub-agent isolation); `merge` keeps newer timestamps.

use crate::core::types::{FileState, FileStateCache};

const MAX_ENTRIES: usize = 100;
const MAX_TOTAL_BYTES: usize = 25 * 1024 * 1024;

impl FileStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn byte_size(state: &FileState) -> usize {
        state.content.len()
    }

    fn remove_silent(&mut self, key: &str) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == key) {
            let (_, state) = self.entries.remove(pos);
            self.total_bytes -= Self::byte_size(&state);
        }
    }

    fn evict(&mut self) {
        while self.entries.len() > MAX_ENTRIES {
            let oldest = self.entries.first().map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                self.remove_silent(&k);
            } else {
                break;
            }
        }
        while self.total_bytes > MAX_TOTAL_BYTES && !self.entries.is_empty() {
            let oldest = self.entries.first().map(|(k, _)| k.clone()).unwrap();
            self.remove_silent(&oldest);
        }
    }

    pub fn get(&mut self, path: &str) -> Option<FileState> {
        let key = crate::files::utils::normalize_path(path);
        let pos = self.entries.iter().position(|(k, _)| *k == key)?;
        // touch: move to MRU (tail)
        let entry = self.entries.remove(pos);
        self.entries.push(entry);
        self.entries.last().map(|(_, s)| s.clone())
    }

    pub fn set(&mut self, path: &str, state: FileState) {
        let key = crate::files::utils::normalize_path(path);
        self.remove_silent(&key);
        self.total_bytes += Self::byte_size(&state);
        self.entries.push((key, state));
        self.evict();
    }

    pub fn has(&self, path: &str) -> bool {
        let key = crate::files::utils::normalize_path(path);
        self.entries.iter().any(|(k, _)| *k == key)
    }

    pub fn delete(&mut self, path: &str) {
        let key = crate::files::utils::normalize_path(path);
        self.remove_silent(&key);
    }

    /// Keys in LRU order (oldest first), like TS `keys()`.
    pub fn keys(&self) -> Vec<String> {
        self.entries.iter().map(|(k, _)| k.clone()).collect()
    }

    pub fn size(&self) -> usize {
        self.entries.len()
    }

    /// Deep clone (sub-agent isolation).
    pub fn deep_clone(&self) -> FileStateCache {
        let mut copy = FileStateCache::default();
        for (k, s) in &self.entries {
            copy.set(k, s.clone());
        }
        copy
    }

    /// Merge another cache; newer timestamps win on conflicts.
    pub fn merge(&mut self, other: &FileStateCache) {
        for (key, state) in other.entries.iter().map(|(k, s)| (k.clone(), s.clone())) {
            let existing = self.entries.iter().find(|(k, _)| *k == key).map(|(_, s)| s.timestamp);
            match existing {
                Some(ts) if state.timestamp <= ts => {}
                _ => self.set(&key, state),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from the cache semantics in the TS implementation
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn state(content: &str, ts: f64) -> FileState {
        FileState {
            content: content.to_string(),
            timestamp: ts,
            offset: None,
            limit: None,
            is_partial_view: None,
        }
    }

    #[test]
    fn set_get_roundtrip_and_touch() {
        let mut c = FileStateCache::new();
        c.set("/tmp/a.rs", state("A", 1.0));
        c.set("/tmp/b.rs", state("B", 2.0));
        assert_eq!(c.get("/tmp/a.rs").unwrap().content, "A");
        // a is now MRU; evicting by count would drop b first
        assert_eq!(c.keys()[1], "/tmp/a.rs");
    }

    #[test]
    fn normalizes_keys() {
        let mut c = FileStateCache::new();
        c.set("/tmp/x/./y.rs", state("X", 1.0));
        assert!(c.has("/tmp/x/y.rs"));
    }

    #[test]
    fn evicts_by_count() {
        let mut c = FileStateCache::new();
        for i in 0..110 {
            c.set(&format!("/tmp/f{i}.txt"), state("x", i as f64));
        }
        assert!(c.size() <= 100);
        assert!(!c.has("/tmp/f0.txt")); // oldest evicted
        assert!(c.has("/tmp/f109.txt"));
    }

    #[test]
    fn evicts_by_total_size() {
        let mut c = FileStateCache::new();
        let big = "x".repeat(5 * 1024 * 1024); // 5MB
        for i in 0..7 {
            c.set(&format!("/tmp/big{i}"), state(&big, i as f64));
        }
        assert!(c.total_bytes <= MAX_TOTAL_BYTES);
    }

    #[test]
    fn merge_keeps_newer_timestamp() {
        let mut base = FileStateCache::new();
        base.set("/f", state("old", 1.0));
        let mut other = FileStateCache::new();
        other.set("/f", state("new", 2.0));
        base.merge(&other);
        assert_eq!(base.get("/f").unwrap().content, "new");

        let mut base2 = FileStateCache::new();
        base2.set("/f", state("newer", 5.0));
        base2.merge(&other);
        assert_eq!(base2.get("/f").unwrap().content, "newer");
    }

    #[test]
    fn deep_clone_is_independent() {
        let mut c = FileStateCache::new();
        c.set("/f", state("one", 1.0));
        let mut copy = c.deep_clone();
        copy.set("/f", state("two", 2.0));
        assert_eq!(c.get("/f").unwrap().content, "one");
    }

    #[test]
    fn delete_works() {
        let mut c = FileStateCache::new();
        c.set("/f", state("x", 1.0));
        c.delete("/f");
        assert!(!c.has("/f"));
        assert_eq!(c.size(), 0);
    }
}
