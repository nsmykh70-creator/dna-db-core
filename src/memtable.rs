// SPDX-License-Identifier: Apache-2.0
//! Concurrent In-Memory Table (MemTable)
//!
//! 1. Memory tracking (approximate) to trigger flushes before unbounded growth
//! 2. Tombstones so flushed segments know a key was deleted
//! 3. Lock-free SkipList (crossbeam) for high-concurrency reads/writes
//! 4. Sorted iteration + prefix range scans for chromosome queries

use std::ops::Bound;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use crossbeam_skiplist::SkipMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryValue {
    Value(Vec<u8>),
    Tombstone,
}

impl EntryValue {
    /// Test convenience: owned value bytes (empty for tombstones).
    pub fn unwrap_value(&self) -> Vec<u8> {
        match self {
            EntryValue::Value(v) => v.clone(),
            EntryValue::Tombstone => Vec::new(),
        }
    }
}

pub struct MemTable {
    map: Arc<SkipMap<Vec<u8>, EntryValue>>,
    approx_size: AtomicIsize,
    capacity: usize,
}

impl MemTable {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: Arc::new(SkipMap::new()),
            approx_size: AtomicIsize::new(0),
            capacity,
        }
    }

    /// Insert/update. Returns true when the table reached capacity.
    pub fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool {
        let mut delta = (key.len() + value.len() + 32) as isize;
        if let Some(old) = self.map.get(&key) {
            match old.value() {
                EntryValue::Value(v) => delta -= (key.len() + v.len() + 32) as isize,
                EntryValue::Tombstone => delta -= (key.len() + 32) as isize,
            }
        }
        self.map.insert(key, EntryValue::Value(value));
        let new_size = self.approx_size.fetch_add(delta, Ordering::Relaxed) as isize + delta;
        new_size >= self.capacity as isize
    }

    /// Insert tombstone. Returns true when the table reached capacity.
    pub fn delete(&self, key: Vec<u8>) -> bool {
        let mut delta = (key.len() + 32) as isize;
        if let Some(old) = self.map.get(&key) {
            match old.value() {
                EntryValue::Value(v) => delta -= (key.len() + v.len() + 32) as isize,
                EntryValue::Tombstone => delta -= (key.len() + 32) as isize,
            }
        }
        self.map.insert(key, EntryValue::Tombstone);
        let new_size = self.approx_size.fetch_add(delta, Ordering::Relaxed) as isize + delta;
        new_size >= self.capacity as isize
    }

    /// Ok(Some(value)) | Ok(None) for tombstone | Err-free: Option<Option<..>>
    pub fn get(&self, key: &[u8]) -> Option<Option<Vec<u8>>> {
        self.map.get(key).map(|entry| match entry.value() {
            EntryValue::Value(v) => Some(v.clone()),
            EntryValue::Tombstone => None,
        })
    }

    pub fn contains_tombstone(&self, key: &[u8]) -> bool {
        matches!(
            self.map.get(key).map(|e| match e.value() {
                EntryValue::Tombstone => 1u8,
                _ => 0u8,
            }),
            Some(1u8)
        )
    }

    pub fn size(&self) -> usize {
        self.approx_size.load(Ordering::Relaxed).max(0) as usize
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.size() >= self.capacity
    }

    pub fn iter(&self) -> impl Iterator<Item = (Vec<u8>, EntryValue)> + '_ {
        self.map.iter().map(|e| (e.key().clone(), e.value().clone()))
    }

    /// Sorted scan of all entries whose key starts with `prefix`.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, EntryValue)> {
        let lo = Bound::Included(prefix.to_vec());
        let hi = {
            // prefix upper bound: increment last byte with carry
            let mut upper = prefix.to_vec();
            loop {
                match upper.last_mut() {
                    Some(b) if *b < 0xFF => {
                        *b += 1;
                        break;
                    }
                    Some(_) => {
                        upper.pop();
                    }
                    None => break,
                }
            }
            if upper.is_empty() {
                Bound::Unbounded
            } else {
                Bound::Excluded(upper)
            }
        };
        self.map
            .range((lo, hi))
            .filter(|e| e.key().starts_with(prefix))
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete() {
        let m = MemTable::new(1000);
        assert_eq!(m.get(b"k"), None);
        m.put(b"k".to_vec(), b"v1".to_vec());
        assert_eq!(m.get(b"k"), Some(Some(b"v1".to_vec())));
        m.put(b"k".to_vec(), b"v2".to_vec());
        assert_eq!(m.get(b"k"), Some(Some(b"v2".to_vec())));
        m.delete(b"k".to_vec());
        assert_eq!(m.get(b"k"), Some(None)); // tombstone
    }

    #[test]
    fn capacity_signal_and_size_accounting() {
        let m = MemTable::new(100);
        for i in 0..10u32 {
            let key = format!("key_{}", i);
            let val = vec![b'A'; 8];
            m.put(key.into_bytes(), val);
        }
        // 10 * (5 + 8 + 32) = 450 > 100 => full
        assert!(m.is_full());
        // Overwrite same key twice should not double-count
        let before = m.size();
        m.put(b"key_0".to_vec(), b"XXXXXXXX".to_vec());
        assert_eq!(m.size(), before);
    }

    #[test]
    fn sorted_iteration() {
        let m = MemTable::new(10_000);
        for k in ["b", "a", "c", "aa"] {
            m.put(k.as_bytes().to_vec(), b"v".to_vec());
        }
        let keys: Vec<_> = m.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![b"a".to_vec(), b"aa".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn prefix_scan() {
        let m = MemTable::new(10_000);
        for k in ["chr1:1", "chr1:2", "chr2:1", "chr10:1", "other"] {
            m.put(k.as_bytes().to_vec(), b"v".to_vec());
        }
        let mut got: Vec<String> = m
            .scan_prefix(b"chr1:")
            .into_iter()
            .map(|(k, _)| String::from_utf8(k).unwrap())
            .collect();
        got.sort();
        assert_eq!(got, vec!["chr1:1", "chr1:2"]);
        assert_eq!(m.scan_prefix(b"chr1:").len(), 2);
    }

    #[test]
    fn prefix_scan_boundary() {
        let m = MemTable::new(10_000);
        for k in ["ab", "abc", "abd", "abe", "ac"] {
            m.put(k.as_bytes().to_vec(), b"v".to_vec());
        }
        let got: Vec<String> = m
            .scan_prefix(b"ab")
            .into_iter()
            .map(|(k, _)| String::from_utf8(k).unwrap())
            .collect();
        assert_eq!(got, vec!["ab", "abc", "abd", "abe"]);
    }
}
