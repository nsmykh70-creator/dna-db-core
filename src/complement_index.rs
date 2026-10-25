// SPDX-License-Identifier: Apache-2.0
//! Complement Index — O(1) foreign-key navigation ("Base-Pairing Complementarity")
//!
//! A real secondary hash index: maps (chromosome, column, encoded value) ->
//! locus key so that LINK VIA COMPLEMENT resolves through a direct key seek,
//! NOT a table scan. The index is maintained by the relational layer on
//! INSERT/UPDATE/DELETE and rebuilt on open from table contents.

use std::collections::{HashMap, HashSet};
use crate::relational::{encode_index_value, Value};

#[derive(Default)]
pub struct ComplementIndex {
    /// (table, column) -> encoded_value -> all matching locus keys.
    index: HashMap<(String, String), HashMap<Vec<u8>, HashSet<String>>>,
    hits: u64,
    misses: u64,
}

impl ComplementIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_column(&mut self, table: &str, column: &str) {
        self.index.entry((table.to_string(), column.to_string())).or_default();
    }

    /// Index one row's FK value -> its locus key.
    pub fn upsert(&mut self, table: &str, column: &str, row_key: &str, value: &Value) {
        let enc = encode_index_value(value);
        self.index
            .entry((table.to_string(), column.to_string()))
            .or_default()
            .entry(enc)
            .or_default()
            .insert(row_key.to_string());
    }

    /// Remove one row's FK mapping (exact value must be provided by caller
    /// from the row's stored field).
    pub fn remove(&mut self, table: &str, column: &str, row_key: &str, value: &Value) {
        let enc = encode_index_value(value);
        if let Some(m) = self.index.get_mut(&(table.to_string(), column.to_string())) {
            if let Some(keys) = m.get_mut(&enc) {
                keys.remove(row_key);
                if keys.is_empty() {
                    m.remove(&enc);
                }
            }
        }
    }

    /// O(1) lookup: value -> locus key of the referencing row.
    pub fn lookup(&mut self, table: &str, column: &str, value: &Value) -> Option<&str> {
        let enc = encode_index_value(value);
        let m = self.index.get(&(table.to_string(), column.to_string()))?;
        match m.get(&enc).and_then(|keys| keys.iter().min()) {
            Some(key) => {
                self.hits += 1;
                Some(key.as_str())
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Return every referencing row for a value. `lookup` remains a convenient
    /// single-result API for callers that only need one matching locus.
    pub fn lookup_all(&mut self, table: &str, column: &str, value: &Value) -> Vec<String> {
        let enc = encode_index_value(value);
        match self
            .index
            .get(&(table.to_string(), column.to_string()))
            .and_then(|values| values.get(&enc))
        {
            Some(keys) => {
                self.hits += keys.len() as u64;
                let mut out: Vec<String> = keys.iter().cloned().collect();
                out.sort();
                out
            }
            None => {
                self.misses += 1;
                Vec::new()
            }
        }
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    pub fn column_count(&self) -> usize {
        self.index.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_roundtrip() {
        let mut idx = ComplementIndex::new();
        idx.register_column("chr_users", "user_id");
        idx.upsert("chr_users", "user_id", "chr_users:14", &Value::Int(14));
        idx.upsert("chr_users", "user_id", "chr_users:18", &Value::Int(18));

        assert_eq!(
            idx.lookup("chr_users", "user_id", &Value::Int(14)),
            Some("chr_users:14")
        );
        assert_eq!(idx.lookup("chr_users", "user_id", &Value::Int(99)), None);
    }

    #[test]
    fn remove_and_reinsert() {
        let mut idx = ComplementIndex::new();
        idx.register_column("chr_orders", "order_id");
        idx.upsert("chr_orders", "order_id", "chr_orders:1", &Value::Int(1));
        idx.remove("chr_orders", "order_id", "chr_orders:1", &Value::Int(1));
        assert_eq!(idx.lookup("chr_orders", "order_id", &Value::Int(1)), None);
        idx.upsert("chr_orders", "order_id", "chr_orders:2", &Value::Int(1));
        assert_eq!(
            idx.lookup("chr_orders", "order_id", &Value::Int(1)),
            Some("chr_orders:2")
        );
    }

    #[test]
    fn text_and_dna_values() {
        let mut idx = ComplementIndex::new();
        idx.register_column("chr_variants", "name");
        idx.upsert("chr_variants", "name", "chr_variants:7", &Value::Text("BRCA1".into()));
        idx.upsert("chr_variants", "name", "chr_variants:8", &Value::Dna("ACGT".into()));
        assert_eq!(
            idx.lookup("chr_variants", "name", &Value::Text("BRCA1".into())),
            Some("chr_variants:7")
        );
        assert_eq!(
            idx.lookup("chr_variants", "name", &Value::Dna("ACGT".into())),
            Some("chr_variants:8")
        );
    }

    #[test]
    fn duplicate_values_keep_all_rows_and_remove_independently() {
        let mut idx = ComplementIndex::new();
        idx.register_column("chr_orders", "user_id");
        idx.upsert("chr_orders", "user_id", "chr_orders:1", &Value::Int(12));
        idx.upsert("chr_orders", "user_id", "chr_orders:2", &Value::Int(12));
        assert_eq!(
            idx.lookup_all("chr_orders", "user_id", &Value::Int(12)),
            vec!["chr_orders:1".to_string(), "chr_orders:2".to_string()]
        );
        idx.remove("chr_orders", "user_id", "chr_orders:1", &Value::Int(12));
        assert_eq!(
            idx.lookup_all("chr_orders", "user_id", &Value::Int(12)),
            vec!["chr_orders:2".to_string()]
        );
    }
}
