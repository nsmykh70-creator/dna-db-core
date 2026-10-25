// SPDX-License-Identifier: AGPL-3.0-only
//! Deterministic, serializable Bloom filter (double hashing, k=4 probes).
//!
//! Replaced the `fastbloom` dependency: this implementation has fixed,
//! reproducible seeds so the filter can be persisted inside SSTable sidecars
//! and reloaded bit-for-bit. Pure auditable code, no unsafe.

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;
const GOLDEN: u64 = 0x9e3779b97f4a7c15;

fn fnv1a64(data: &[u8], seed: u64) -> u64 {
    let mut h = FNV_OFFSET ^ seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bloom {
    bits: Vec<u64>,
    num_bits: u64,
}

impl Bloom {
    /// Create a filter sized for `expected_items` (~10 bits/item, ~1% FPR).
    pub fn new(expected_items: usize) -> Self {
        let num_bits: u64 = ((expected_items as u64).saturating_mul(10))
            .max(64)
            .next_multiple_of(64);
        Self {
            bits: vec![0u64; (num_bits / 64) as usize],
            num_bits,
        }
    }

    /// Create a permissive filter (all bits set) — used when the sidecar is
    /// missing: every lookup proceeds to disk (correct, just slower).
    pub fn permissive() -> Self {
        Self {
            bits: vec![u64::MAX; 1],
            num_bits: 64,
        }
    }

    #[inline]
    fn probes(&self, key: &[u8]) -> [u64; 4] {
        let h1 = fnv1a64(key, 0);
        let h2 = fnv1a64(key, GOLDEN) | 1; // odd => full period
        [
            h1,
            h1.wrapping_add(h2),
            h1.wrapping_add(h2.wrapping_mul(2)),
            h1.wrapping_add(h2.wrapping_mul(3)),
        ]
    }

    #[inline]
    fn bit_index(&self, p: u64) -> usize {
        (p % self.num_bits) as usize
    }

    pub fn insert(&mut self, key: &[u8]) {
        for p in self.probes(key) {
            let idx = self.bit_index(p);
            self.bits[idx / 64] |= 1u64 << (idx % 64);
        }
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        self.probes(key).iter().all(|&p| {
            let idx = self.bit_index(p);
            self.bits[idx / 64] & (1u64 << (idx % 64)) != 0
        })
    }

    /// Serialize: [num_bits u64][words u64][words...]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.bits.len() * 8);
        out.extend_from_slice(&self.num_bits.to_le_bytes());
        out.extend_from_slice(&(self.bits.len() as u64).to_le_bytes());
        for w in &self.bits {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, String> {
        if buf.len() < 16 {
            return Err("bloom: buffer too short".into());
        }
        let num_bits = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let words = u64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize;
        if words > 1_048_576 || buf.len() < 16 + words * 8 {
            return Err("bloom: invalid word count".into());
        }
        let mut bits = Vec::with_capacity(words);
        for i in 0..words {
            let off = 16 + i * 8;
            bits.push(u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()));
        }
        Ok(Self { bits, num_bits })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_contains() {
        let mut b = Bloom::new(1000);
        for i in 0..1000u32 {
            b.insert(&i.to_le_bytes());
        }
        for i in 0..1000u32 {
            assert!(b.contains(&i.to_le_bytes()));
        }
    }

    #[test]
    fn false_positive_rate_bounded() {
        let mut b = Bloom::new(1000);
        for i in 0..1000u32 {
            b.insert(&i.to_le_bytes());
        }
        let mut fp = 0;
        for i in 1000..3000u32 {
            if b.contains(&i.to_le_bytes()) {
                fp += 1;
            }
        }
        let rate = fp as f64 / 2000.0;
        assert!(rate < 0.05, "FPR too high: {}", rate);
    }

    #[test]
    fn serialize_roundtrip() {
        let mut b = Bloom::new(500);
        for i in 0..500u32 {
            b.insert(&i.to_le_bytes());
        }
        let bytes = b.to_bytes();
        let b2 = Bloom::from_bytes(&bytes).unwrap();
        assert_eq!(b, b2);
        for i in 0..500u32 {
            assert!(b2.contains(&i.to_le_bytes()));
        }
    }

    #[test]
    fn rejects_corrupt() {
        assert!(Bloom::from_bytes(&[0u8; 8]).is_err());
        let b = Bloom::new(64);
        let mut bytes = b.to_bytes();
        bytes[8] = 0xFF; // absurd word count
        assert!(Bloom::from_bytes(&bytes).is_err());
    }
}
