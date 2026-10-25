//! Reed–Solomon erasure codec over GF(2^8) — the "genome redundancy" layer.
//!
//! Wraps the battle-tested `reed-solomon-erasure` crate (Backblaze Cauchy
//! matrix, field polynomial 0x11D) behind a narrow, auditable API:
//!
//!   [d0 .. d(k-1)]  k data shards, each exactly `shard_size` bytes
//!   [p0 .. p(m-1)]  m parity shards, computed by [`ErasureCodec::encode`]
//!
//! Any `m` shards of a stripe may be lost; [`ErasureCodec::reconstruct`]
//! restores them bit-for-bit from the surviving `k`. Acceptance tests verify
//! the fundamental RS contract — data shards are identity-encoded, so
//! reconstruction MUST reproduce the original data bytes exactly, for every
//! (k, m) configuration and every damage pattern ≤ m.

use reed_solomon_erasure::galois_8::ReedSolomon;

/// Errors surfaced by the codec.
#[derive(Debug)]
pub enum ErasureError {
    /// Fewer than `k` surviving shards — mathematically unrecoverable.
    InsufficientShards { present: usize, needed: usize },
    /// Shards had inconsistent lengths or an out-of-range configuration.
    InvalidShape(&'static str),
    /// Underlying codec failure.
    Codec(String),
}

impl std::fmt::Display for ErasureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErasureError::InsufficientShards { present, needed } => write!(
                f,
                "insufficient shards for reconstruction: {} present, {} needed",
                present, needed
            ),
            ErasureError::InvalidShape(why) => write!(f, "invalid shard shape: {}", why),
            ErasureError::Codec(why) => write!(f, "reed-solomon codec error: {}", why),
        }
    }
}

impl std::error::Error for ErasureError {}

/// Reed–Solomon codec for one fixed (k, m) geometry.
pub struct ErasureCodec {
    k: usize,
    m: usize,
    rs: ReedSolomon,
}

impl ErasureCodec {
    /// Build a codec. `k` data shards + `m` parity shards, both ≥ 1.
    pub fn new(k: usize, m: usize) -> Result<Self, ErasureError> {
        if k == 0 || m == 0 {
            return Err(ErasureError::InvalidShape("k and m must both be >= 1"));
        }
        if k + m > 256 {
            return Err(ErasureError::InvalidShape("k + m must be <= 256 for GF(2^8)"));
        }
        let rs = ReedSolomon::new(k, m)
            .map_err(|e| ErasureError::Codec(format!("ReedSolomon::new({k}, {m}): {e}")))?;
        Ok(Self { k, m, rs })
    }

    pub fn data_shards(&self) -> usize {
        self.k
    }

    pub fn parity_shards(&self) -> usize {
        self.m
    }

    /// Encode `k` equal-length data shards, returning `m` parity shards.
    pub fn encode(&self, data: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, ErasureError> {
        if data.len() != self.k {
            return Err(ErasureError::InvalidShape("encode expects exactly k data shards"));
        }
        let shard_len = data[0].len();
        if data.iter().any(|d| d.len() != shard_len) {
            return Err(ErasureError::InvalidShape("data shards must have equal length"));
        }
        // Crate 6.0 API: one (k+m)-slice, parity slots overwritten in place.
        let mut all: Vec<Vec<u8>> = Vec::with_capacity(self.k + self.m);
        all.extend(data.iter().cloned());
        all.extend((0..self.m).map(|_| vec![0u8; shard_len]));
        self.rs
            .encode(&mut all)
            .map_err(|e| ErasureError::Codec(format!("encode: {e}")))?;
        Ok(all.split_off(self.k))
    }

    /// Reconstruct missing shards in-place. `shards` holds `k + m` slots
    /// (`None` = lost). On success every slot is `Some` and the surviving
    /// shards are untouched.
    pub fn reconstruct(&self, shards: &mut [Option<Vec<u8>>]) -> Result<(), ErasureError> {
        if shards.len() != self.k + self.m {
            return Err(ErasureError::InvalidShape("reconstruct expects k+m slots"));
        }
        let present = shards.iter().filter(|s| s.is_some()).count();
        if present < self.k {
            return Err(ErasureError::InsufficientShards {
                present,
                needed: self.k,
            });
        }
        // All present shards must agree on length.
        let shard_len = shards
            .iter()
            .flatten()
            .next()
            .map(|s| s.len())
            .ok_or(ErasureError::InvalidShape("no shards present"))?;
        if shards
            .iter()
            .flatten()
            .any(|s| s.len() != shard_len)
        {
            return Err(ErasureError::InvalidShape("surviving shards disagree on length"));
        }
        self.rs
            .reconstruct(shards)
            .map_err(|e| ErasureError::Codec(format!("reconstruct: {e}")))
        // Post-condition (checked by tests): every slot is now Some.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        // xorshift64* — deterministic filler with good byte coverage.
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state.wrapping_mul(0x2545F4914F6CDD1D) >> 24) as u8
            })
            .collect()
    }

    fn make_stripe(k: usize, shard_len: usize, seed: u64) -> Vec<Vec<u8>> {
        (0..k).map(|i| pseudo_random(shard_len, seed + i as u64)).collect()
    }

    /// ACCEPTANCE (P2.1): encode → erase up to m shards → reconstruct →
    /// every original byte is back, no matter which shards died.
    #[test]
    fn acceptance_roundtrip_all_damage_patterns() {
        for &(k, m) in &[(1usize, 1usize), (2, 2), (4, 2), (4, 4), (8, 4), (6, 3)] {
            let codec = ErasureCodec::new(k, m).unwrap();
            let data = make_stripe(k, 4096, 0xAB00 + k as u64);
            let parity = codec.encode(&data).unwrap();
            assert_eq!(parity.len(), m);

            let slots: Vec<Option<Vec<u8>>> =
                data.iter().cloned().map(Some).chain(parity.into_iter().map(Some)).collect();

            // Erase every possible combination of up to m shard positions.
            let total = k + m;
            for mask in 1u32..(1u32 << total) {
                if mask.count_ones() as usize > m {
                    continue;
                }
                let mut damaged = slots.clone();
                for i in 0..total {
                    if mask & (1 << i) != 0 {
                        damaged[i] = None;
                    }
                }
                codec.reconstruct(&mut damaged).unwrap();
                for (i, slot) in damaged.iter().enumerate() {
                    let expected = slots[i].as_ref().unwrap();
                    assert_eq!(
                        slot.as_deref(),
                        Some(expected.as_slice()),
                        "k={} m={} erased mask={:#b} shard {} mismatch",
                        k, m, mask, i
                    );
                }
            }
        }
    }

    /// m+1 lost shards must be reported as unrecoverable — never silently
    /// "reconstructed" into garbage.
    #[test]
    fn over_damage_is_honest() {
        let codec = ErasureCodec::new(4, 2).unwrap();
        let data = make_stripe(4, 512, 7);
        let mut slots: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        slots.push(None);
        slots.push(None);
        slots[0] = None;
        slots[2] = None;
        slots[4] = None; // 3 lost > m=2
        match codec.reconstruct(&mut slots) {
            Err(ErasureError::InsufficientShards { present, needed }) => {
                // Survivors: data[1], data[3] and nothing else.
                assert_eq!(present, 2);
                assert_eq!(needed, 4);
            }
            other => panic!("expected InsufficientShards, got {:?}", other.map(|_| ()).err()),
        }
    }

    /// Parity determinism: same input → same parity (Cauchy matrix, GF(2^8)).
    #[test]
    fn parity_is_deterministic() {
        let codec = ErasureCodec::new(4, 2).unwrap();
        let data = make_stripe(4, 4096, 99);
        let p1 = codec.encode(&data).unwrap();
        let p2 = codec.encode(&data).unwrap();
        assert_eq!(p1, p2);
    }

    /// Golden shape checks.
    #[test]
    fn rejects_bad_geometry() {
        assert!(ErasureCodec::new(0, 2).is_err());
        assert!(ErasureCodec::new(4, 0).is_err());
        assert!(ErasureCodec::new(200, 100).is_err());
        assert!(ErasureCodec::new(4, 2).is_ok());
    }

    /// Uneven shard lengths are rejected before any math happens.
    #[test]
    fn rejects_ragged_input() {
        let codec = ErasureCodec::new(4, 2).unwrap();
        let data = vec![vec![1u8; 16], vec![2u8; 16], vec![3u8; 15], vec![4u8; 16]];
        assert!(codec.encode(&data).is_err());
    }
}
