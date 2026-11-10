// SPDX-License-Identifier: AGPL-3.0-only
//! Relational Schema Mapping via the DNA Base-Pairing Metaphor
//!
//! Tables -> Chromosomes ('chr_name:locus_id' LSM namespace)
//! Rows   -> Loci
//! FK     -> Complementary base-pairing (O(1) via ComplementIndex)
//!
//! v2.1.0 adds:
//! - Binary typed RowCodec (tag-length-value) — no JSON in the storage path
//! - Value total ordering (for WHERE comparisons and index encoding)
//! - Schema persistence (schema rows stored under '<table>:schema')

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ColumnType {
    Int64,
    Float64,
    Text,
    Dna2Bit,
    Timestamp,
    ForeignLink {
        target_chromosome: String,
        target_column: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Int(i64),
    Float(f64),
    Text(String),
    Dna(String),
    Null,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{}", i),
            Value::Float(x) => write!(f, "{}", x),
            Value::Text(s) => write!(f, "{}", s),
            Value::Dna(s) => write!(f, "{}", s),
            Value::Null => write!(f, "NULL"),
        }
    }
}

impl Value {
    /// Total ordering for comparisons (NULL < Int < Float < Text < Dna).
    ///
    /// CMS edition (branch `cms-edition`): numeric coercion for mixed
    /// Int/Float vs Text — MySQL compares numbers and numeric strings
    /// numerically (`WHERE id = '2'` matches the row with id 2). WordPress
    /// relies on this: `WP_User::get_data_by()` sends `WHERE ID = %s`.
    /// Non-numeric text keeps the rank ordering (Text > numbers) instead of
    /// MySQL's "any string casts to 0" rule, which is a known footgun.
    pub fn compare(&self, other: &Value) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Null => 0,
                Value::Int(_) | Value::Float(_) => 1,
                Value::Text(_) => 2,
                Value::Dna(_) => 3,
            }
        }
        fn numeric(v: &Value) -> Option<f64> {
            match v {
                Value::Int(i) => Some(*i as f64),
                Value::Float(f) => Some(*f),
                Value::Text(s) | Value::Dna(s) => s.trim().parse::<f64>().ok(),
                Value::Null => None,
            }
        }
        // Numeric coercion: number vs numeric-looking string (either side —
        // a Text/Dna-stored column value vs an Int literal, or the reverse).
        if !matches!(self, Value::Null) && !matches!(other, Value::Null) {
            if let (Some(a), Some(b)) = (numeric(self), numeric(other)) {
                // Only apply when at least one side is text — two numbers
                // (or number vs float) already compare in the match below.
                if matches!(self, Value::Text(_) | Value::Dna(_))
                    || matches!(other, Value::Text(_) | Value::Dna(_))
                {
                    return a.partial_cmp(&b).unwrap_or(Equal);
                }
            }
        }
        let (ra, rb) = (rank(self), rank(other));
        if ra != rb {
            return ra.cmp(&rb);
        }
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(Equal),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b).unwrap_or(Equal),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Equal),
            (Value::Text(a), Value::Text(b)) | (Value::Dna(a), Value::Dna(b)) => a.cmp(b),
            _ => Equal,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Leading numeric prefix of the value per MySQL string-to-number
    /// rules ("150abc" → 150, "abc" → None). Single source of truth for
    /// CAST, arithmetic coercion and CMS insert/update type coercion.
    pub fn mysql_numeric_prefix(&self) -> Option<f64> {
        let s = match self {
            Value::Int(i) => return Some(*i as f64),
            Value::Float(f) => return Some(*f),
            Value::Text(s) | Value::Dna(s) => s,
            Value::Null => return None,
        };
        let t = s.trim_start();
        let bytes = t.as_bytes();
        let mut i = 0;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i == digits_start {
            return None; // no digits at all
        }
        // Optional exponent (only if at least one digit precedes it).
        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
            let save = i;
            i += 1;
            if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                i += 1;
            }
            if i < bytes.len() && bytes[i].is_ascii_digit() {
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            } else {
                i = save; // 'e' without exponent digits — not part of number
            }
        }
        t[..i].parse::<f64>().ok()
    }

    /// Coerce a value to the declared column type the way MariaDB does on
    /// write — `$wpdb->insert()` without a format array sends EVERY value
    /// as a string, so `order_id` reaches a `bigint unsigned` column as
    /// '15'. MySQL converts on store; without the same conversion the
    /// secondary-index key (built from the stored Text) never matches
    /// lookups by an Int literal (`IndexScan → 0 rows`) and JOINs on the
    /// column go blind. Non-numeric text becomes 0 (the non-strict mode
    /// WordPress runs with, `sql_mode=''`). Since v4.3 the SQL grammar is
    /// shared by both builds — ALTER TABLE MODIFY COLUMN (data rewrite)
    /// uses this outside the cms feature too.
    pub fn coerce_for_column(ct: &ColumnType, v: Value) -> Value {
        match ct {
            ColumnType::Int64 => match v {
                Value::Int(_) | Value::Null => v,
                other => {
                    let f = other.mysql_numeric_prefix().unwrap_or(0.0);
                    let rounded = f.round();
                    let i = if rounded >= i64::MAX as f64 {
                        i64::MAX
                    } else if rounded <= i64::MIN as f64 {
                        i64::MIN
                    } else {
                        rounded as i64
                    };
                    Value::Int(i)
                }
            },
            ColumnType::Float64 => match v {
                Value::Float(_) | Value::Null => v,
                Value::Int(i) => Value::Float(i as f64),
                other => Value::Float(other.mysql_numeric_prefix().unwrap_or(0.0)),
            },
            // TEXT / DNA / TIMESTAMP / FK keep the value as-is (already the
            // engine's storage representation for string-ish columns).
            _ => v,
        }
    }

    /// SQL-ish name of the value's variant (SHOW STORAGE / storage maps).
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "INT",
            Value::Float(_) => "FLOAT",
            Value::Text(_) => "TEXT",
            Value::Dna(_) => "DNA",
            Value::Null => "NULL",
        }
    }

    /// v4.1: human-readable description of the physical (RowCodec)
    /// representation of this value — the "stored_as" column of the
    /// SHOW STORAGE / STORAGE_MAP reports. Honest down to the tag bytes.
    pub fn stored_as(&self) -> String {
        match self {
            Value::Null => "RowCodec tag 0: NULL".to_string(),
            Value::Int(i) => {
                format!("RowCodec tag 1: i64 little-endian (8 B) = {}", i)
            }
            Value::Float(x) => format!(
                "RowCodec tag 2: f64 IEEE-754 little-endian (8 B) = {}",
                x
            ),
            Value::Text(s) => format!(
                "RowCodec tag 3: UTF-8 text, u32 length + {} B",
                s.len()
            ),
            Value::Dna(s) => format!(
                "RowCodec tag 4: nucleotide chain A/C/G/T, {} nt ({} B ASCII); \
                 decoded value shown in the value column",
                s.len(),
                s.len()
            ),
        }
    }
}

/// Order-preserving byte encoding for secondary-index keys.
/// Lexicographic byte order of the output matches `Value::compare`:
///   tag 0 = NULL, tag 1 = numeric (Int/Float, mixed-comparable),
///   tag 2 = Text, tag 3 = Dna.
/// Ints and floats share one numeric space (Int(5) == Float(5.0)) via
/// f64 conversion; floats use the standard total-order transform
/// (sign-flip / bitwise-invert) so byte order == numeric order.
pub fn encode_ordered_value(v: &Value) -> Vec<u8> {
    fn ordered_f64_bits(x: f64) -> u64 {
        let bits = x.to_bits();
        if bits & (1u64 << 63) != 0 {
            // negative: invert all bits so more-negative sorts lower
            !bits
        } else {
            // positive: flip sign bit so positives sort above negatives
            bits ^ (1u64 << 63)
        }
    }
    match v {
        Value::Null => vec![0],
        Value::Int(i) => {
            let mut out = vec![1];
            out.extend_from_slice(&ordered_f64_bits(*i as f64).to_be_bytes());
            out
        }
        Value::Float(x) => {
            let mut out = vec![1];
            out.extend_from_slice(&ordered_f64_bits(*x).to_be_bytes());
            out
        }
        Value::Text(s) => {
            let mut out = vec![2];
            out.extend_from_slice(s.as_bytes());
            out
        }
        Value::Dna(s) => {
            let mut out = vec![3];
            out.extend_from_slice(s.as_bytes());
            out
        }
    }
}

/// Encode a value into a hashable byte key for the ComplementIndex.
pub fn encode_index_value(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => vec![0],
        Value::Int(i) => {
            let mut out = vec![1];
            out.extend_from_slice(&i.to_le_bytes());
            out
        }
        Value::Float(x) => {
            let mut out = vec![2];
            out.extend_from_slice(&x.to_bits().to_le_bytes());
            out
        }
        Value::Text(s) => {
            let mut out = vec![3];
            out.extend_from_slice(s.as_bytes());
            out
        }
        Value::Dna(s) => {
            let mut out = vec![4];
            out.extend_from_slice(s.as_bytes());
            out
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyConstraint {
    pub column: String,
    pub target_chromosome: String,
    pub target_locus_column: String,
    pub on_delete_cascade: bool,
}

/// Secondary index definition (persisted inside the table schema row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecondaryIndexMeta {
    pub name: String,
    pub column: String,
    /// Enforce SQL UNIQUE semantics on this column (CREATE UNIQUE INDEX).
    /// Older persisted schemas decode with `unique: false`.
    #[serde(default)]
    pub unique: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChromosomeTable {
    pub name: String,
    /// Column order preserved via BTreeMap for deterministic codec output
    pub columns: BTreeMap<String, ColumnType>,
    /// CMS edition (branch cms-edition): column DECLARATION order as
    /// MySQL reports it (`SELECT *`, `t.*`, SHOW COLUMNS). The BTreeMap
    /// above sorts alphabetically; this Vec records the order in which
    /// columns were added (CREATE TABLE list, then ALTER TABLE ADD).
    /// Older persisted schemas decode with an empty Vec — consumers fall
    /// back to the BTreeMap order.
    #[serde(default)]
    pub column_order: Vec<String>,
    pub foreign_keys: Vec<ForeignKeyConstraint>,
    pub primary_locus_column: String,
    /// CMS edition: full PRIMARY KEY column list (composite-aware).
    /// `primary_locus_column` stays the FIRST key part for native paths and
    /// row keys of single-column tables are unchanged. Older persisted
    /// schemas decode with an empty Vec — consumers fall back to
    /// `[primary_locus_column]`.
    #[serde(default)]
    pub primary_key_cols: Vec<String>,
    /// Secondary indexes on non-PK columns (CREATE INDEX).
    /// Older persisted schemas decode with an empty list.
    #[serde(default)]
    pub secondary_indexes: Vec<SecondaryIndexMeta>,
    /// CMS edition (branch cms-edition): name of the AUTO_INCREMENT column
    /// when the table was created with one (`--features cms`). Serde default
    /// keeps older persisted schemas decodable; builds without the cms
    /// feature simply carry `None`.
    #[serde(default)]
    pub auto_increment_column: Option<String>,
    /// CMS edition (branch cms-edition): effective column defaults for
    /// MySQL-dialect tables — the explicit `DEFAULT '…'` literal plus the
    /// implicit zero-values of NOT NULL columns ('' / 0 / zero-date), i.e.
    /// exactly what MySQL fills in when a column is omitted from INSERT.
    /// Populated only by the cms CREATE TABLE grammar; exec_insert_tx fills
    /// omitted columns from this map (`--features cms`). Serde default keeps
    /// older persisted schemas decodable.
    #[serde(default)]
    pub column_defaults: BTreeMap<String, String>,
}

impl ChromosomeTable {
    pub fn new(name: impl Into<String>, pk_col: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            columns: BTreeMap::new(),
            column_order: Vec::new(),
            foreign_keys: Vec::new(),
            primary_locus_column: pk_col.into(),
            primary_key_cols: Vec::new(),
            secondary_indexes: Vec::new(),
            auto_increment_column: None,
            column_defaults: BTreeMap::new(),
        }
    }

    pub fn secondary_index_on(&self, column: &str) -> Option<&SecondaryIndexMeta> {
        self.secondary_indexes.iter().find(|i| i.column == column)
    }

    pub fn add_column(&mut self, name: impl Into<String>, col_type: ColumnType) {
        let name = name.into();
        // CMS edition: record declaration order (first insertion wins —
        // ALTER TABLE ADD appends, DROP leaves no ghost entries).
        if !self.column_order.contains(&name) {
            self.column_order.push(name.clone());
        }
        self.columns.insert(name, col_type);
    }

    /// CMS edition: declared column order when recorded, else the
    /// alphabetical BTreeMap order (older persisted schemas).
    pub fn declared_columns(&self) -> Vec<String> {
        if self.column_order.is_empty() {
            self.columns.keys().cloned().collect()
        } else {
            // Keep only columns that still exist (DROP COLUMN syncs the
            // order list, but stay defensive against old snapshots).
            self.column_order
                .iter()
                .filter(|c| self.columns.contains_key(c.as_str()))
                .cloned()
                .collect()
        }
    }

    pub fn add_foreign_key(&mut self, fk: ForeignKeyConstraint) {
        self.foreign_keys.push(fk);
    }

    pub fn column_type(&self, name: &str) -> Option<&ColumnType> {
        self.columns.get(name)
    }

    /// SQL-ish name of a column type (SHOW STORAGE / storage maps).
    /// Defined here so every reporter uses the exact schema wording.
    pub fn column_type_name(&self, column: &str) -> String {
        match self.column_type(column) {
            Some(ColumnType::Int64) => "INT".to_string(),
            Some(ColumnType::Float64) => "FLOAT".to_string(),
            Some(ColumnType::Text) => "TEXT".to_string(),
            Some(ColumnType::Dna2Bit) => "DNA".to_string(),
            Some(ColumnType::Timestamp) => "TIMESTAMP".to_string(),
            Some(ColumnType::ForeignLink {
                target_chromosome,
                target_column,
            }) => format!("FK->{}({})", target_chromosome, target_column),
            None => "?".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Binary RowCodec
// ---------------------------------------------------------------------------
// Row := [field_count u32]{ [klen u32][key][type u8][payload] }*
// type: 0=Null 1=Int(i64 le) 2=Float(f64 le) 3=Text(varint len + bytes)
//       4=Dna(varint len + bytes)
// Text/Dna payload uses a u32 length prefix for simplicity + bounds checks.

pub struct RowCodec;

impl RowCodec {
    pub fn encode(fields: &[(String, Value)]) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        for (key, value) in fields {
            out.extend_from_slice(&(key.len() as u32).to_le_bytes());
            out.extend_from_slice(key.as_bytes());
            match value {
                Value::Null => out.push(0),
                Value::Int(i) => {
                    out.push(1);
                    out.extend_from_slice(&i.to_le_bytes());
                }
                Value::Float(x) => {
                    out.push(2);
                    out.extend_from_slice(&x.to_bits().to_le_bytes());
                }
                Value::Text(s) => {
                    out.push(3);
                    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    out.extend_from_slice(s.as_bytes());
                }
                Value::Dna(s) => {
                    out.push(4);
                    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    out.extend_from_slice(s.as_bytes());
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Vec<(String, Value)>, String> {
        if bytes.len() < 4 {
            return Err("row: too short".into());
        }
        let count = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let max_possible_fields = bytes.len().saturating_sub(4) / 5;
        if count > max_possible_fields {
            return Err("row: field count exceeds the encoded payload".into());
        }
        let mut pos = 4usize;
        let mut out = Vec::with_capacity(count.min(max_possible_fields));

        for _ in 0..count {
            if pos
                .checked_add(4)
                .filter(|end| *end <= bytes.len())
                .is_none()
            {
                return Err("row: truncated key length".into());
            }
            let klen = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            let key_end = pos
                .checked_add(klen)
                .filter(|end| {
                    end.checked_add(1)
                        .map(|tag_end| tag_end <= bytes.len())
                        .unwrap_or(false)
                })
                .ok_or_else(|| "row: truncated key".to_string())?;
            if key_end >= bytes.len() {
                return Err("row: truncated key".into());
            }
            let key = String::from_utf8_lossy(&bytes[pos..key_end]).to_string();
            pos = key_end;
            let tag = bytes[pos];
            pos += 1;
            let value = match tag {
                0 => Value::Null,
                1 => {
                    if pos
                        .checked_add(8)
                        .filter(|end| *end <= bytes.len())
                        .is_none()
                    {
                        return Err("row: truncated int".into());
                    }
                    let v = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
                    pos += 8;
                    Value::Int(v)
                }
                2 => {
                    if pos
                        .checked_add(8)
                        .filter(|end| *end <= bytes.len())
                        .is_none()
                    {
                        return Err("row: truncated float".into());
                    }
                    let v =
                        f64::from_bits(u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap()));
                    pos += 8;
                    Value::Float(v)
                }
                3 | 4 => {
                    if pos
                        .checked_add(4)
                        .filter(|end| *end <= bytes.len())
                        .is_none()
                    {
                        return Err("row: truncated string length".into());
                    }
                    let slen = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                    pos += 4;
                    let end = pos
                        .checked_add(slen)
                        .filter(|end| *end <= bytes.len())
                        .ok_or_else(|| "row: truncated string".to_string())?;
                    let s = String::from_utf8_lossy(&bytes[pos..end]).to_string();
                    pos = end;
                    if tag == 3 {
                        Value::Text(s)
                    } else {
                        Value::Dna(s)
                    }
                }
                other => return Err(format!("row: unknown type tag {}", other)),
            };
            out.push((key, value));
        }
        if pos != bytes.len() {
            return Err("row: trailing bytes after encoded fields".into());
        }
        Ok(out)
    }

    pub fn encode_map(map: &BTreeMap<String, Value>) -> Vec<u8> {
        let fields: Vec<(String, Value)> =
            map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        Self::encode(&fields)
    }

    pub fn decode_map(bytes: &[u8]) -> Result<BTreeMap<String, Value>, String> {
        let fields = Self::decode(bytes)?;
        Ok(fields.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_ordering() {
        assert_eq!(
            Value::Null.compare(&Value::Int(1)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            Value::Int(1).compare(&Value::Int(2)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            Value::Int(5).compare(&Value::Float(5.0)),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            Value::Float(1.5).compare(&Value::Int(2)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            Value::Text("abc".into()).compare(&Value::Text("abd".into())),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn rowcodec_roundtrip_all_types() {
        let fields = vec![
            ("null_f".to_string(), Value::Null),
            ("int_f".to_string(), Value::Int(-42)),
            ("float_f".to_string(), Value::Float(123.456)),
            ("text_f".to_string(), Value::Text("привет мир".to_string())),
            ("dna_f".to_string(), Value::Dna("ACGTACGT".to_string())),
        ];
        let bytes = RowCodec::encode(&fields);
        let decoded = RowCodec::decode(&bytes).unwrap();
        assert_eq!(fields, decoded);
    }

    #[test]
    fn rowcodec_map_roundtrip() {
        let mut map = BTreeMap::new();
        map.insert("user_id".to_string(), Value::Int(14));
        map.insert(
            "username".to_string(),
            Value::Text("prof_crick".to_string()),
        );
        map.insert("balance".to_string(), Value::Float(890.5));
        let bytes = RowCodec::encode_map(&map);
        let decoded = RowCodec::decode_map(&bytes).unwrap();
        assert_eq!(map, decoded);
    }

    #[test]
    fn rowcodec_rejects_corrupt() {
        assert!(RowCodec::decode(&[1, 2, 3]).is_err());
        let mut good = RowCodec::encode(&[("a".into(), Value::Int(1))]);
        good.push(0xEE); // unknown tag appended -> count mismatch tolerated? decode ignores trailing? No:
                         // trailing byte alone doesn't break; corrupt the tag:
        let mut bad = RowCodec::encode(&[("a".into(), Value::Int(1))]);
        let n = bad.len();
        bad[n - 9] = 99; // overwrite type tag
        assert!(RowCodec::decode(&bad).is_err());
        let _ = good;
    }

    #[test]
    fn rowcodec_rejects_untrusted_field_count_without_large_allocation() {
        assert!(RowCodec::decode(&u32::MAX.to_le_bytes()).is_err());
    }

    #[test]
    fn index_encoding_distinguishes_types() {
        assert_ne!(
            encode_index_value(&Value::Int(1)),
            encode_index_value(&Value::Text("1".into()))
        );
        assert_eq!(
            encode_index_value(&Value::Int(14)),
            encode_index_value(&Value::Int(14))
        );
    }

    #[test]
    fn table_schema() {
        let mut t = ChromosomeTable::new("chr_users", "user_id");
        t.add_column("user_id", ColumnType::Int64);
        t.add_column("username", ColumnType::Text);
        t.add_foreign_key(ForeignKeyConstraint {
            column: "user_id".into(),
            target_chromosome: "chr_users".into(),
            target_locus_column: "user_id".into(),
            on_delete_cascade: true,
        });
        assert_eq!(t.column_type("user_id"), Some(&ColumnType::Int64));
        assert_eq!(t.foreign_keys.len(), 1);
    }
}
