//! Bit-Parallel Genomic Alignment Engine (Myers / Wu-Manber Bitap)
//!
//! v2.1.0: correct bit-parallel approximate matching with the standard
//! "match convention" bitmasks (bit i of R[d] set <=> pattern[0..=i] aligns to
//! a substring of the target ending at the current position with <= d edits).
//!
//! - Levenshtein search (substitutions + insertions + deletions), k <= 63
//! - Pure Hamming search (substitutions only), k <= 63
//! - Reports the TRUE edit distance per match (not just the threshold)
//! - Patterns longer than 64 bp are rejected (bit-vector width limit),
//!   see ROADMAP: chunked long-pattern search
//!
//! State initialization: R[d] = (1<<d)-1 encodes "pattern prefix of length
//! i <= d aligned to the empty target via i deletions" (mirrors DP column
//! D[i][0] = i), which is what makes leading/trailing indel alignments findable.
//!
//! Correctness is property-tested below against a naive semi-global DP
//! reference and brute-force Hamming windows (8000 randomized cases).

use serde::{Deserialize, Serialize};
use crate::dna::PackedDna;

pub const MAX_BITAP_PATTERN: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignmentMatch {
    /// Index of the first target nucleotide of the reported alignment window.
    pub offset_bp: usize,
    /// TRUE edit distance (Hamming or Levenshtein depending on search mode).
    pub mismatches: usize,
    /// Target window of the alignment. Guaranteed: edit_distance(pattern,
    /// matched_sequence) == mismatches exactly.
    pub matched_sequence: String,
}

#[inline]
fn base_code(b: u8) -> usize {
    match b {
        b'A' | b'a' => 0,
        b'C' | b'c' => 1,
        b'G' | b'g' => 2,
        _ => 3, // T/t; any other byte also maps to 3 (Peq bit likely 0)
    }
}

fn build_peq(pattern: &str) -> [u64; 4] {
    let mut peq = [0u64; 4];
    for (i, &b) in pattern.as_bytes().iter().enumerate() {
        match b {
            b'A' | b'a' => peq[0] |= 1u64 << i,
            b'C' | b'c' => peq[1] |= 1u64 << i,
            b'G' | b'g' => peq[2] |= 1u64 << i,
            b'T' | b't' => peq[3] |= 1u64 << i,
            _ => {} // invalid chars simply never match
        }
    }
    peq
}

fn prefix_mask(m: usize) -> u64 {
    if m >= 64 { u64::MAX } else { (1u64 << m) - 1 }
}

/// Naive full DP edit distance (m <= 64, only run at MATCH positions).
fn naive_lev(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1)
                .min(cur[j - 1] + 1)
                .min(prev[j - 1] + (a[i - 1] != b[j - 1]) as usize);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Recovers the SHORTEST target window ending at `j` whose edit distance to
/// `pattern` is exactly `d` (guaranteed to exist: bitap found a <= d alignment).
fn best_window(text: &str, pattern: &str, j: usize, d: usize) -> (usize, String) {
    let m = pattern.len();
    let min_len = (m - d).max(1); // lev >= |m - L| => shorter windows cannot match
    let mut start = (j + 1).saturating_sub(min_len);
    loop {
        let win = &text[start..=j];
        if naive_lev(pattern, win) <= d {
            return (start, win.to_string());
        }
        if start == 0 {
            break;
        }
        start -= 1;
    }
    let start = (j + 1).saturating_sub(m + d);
    (start, text[start..=j].to_string())
}

pub struct BitapMatcher;

impl BitapMatcher {
    /// Core bit-parallel Levenshtein scan.
    /// Returns (end_position, true_edit_distance) for every end position where
    /// some substring of `text` ending there has edit distance <= k.
    fn levenshtein_ends(text: &[u8], pattern: &str, k: usize) -> Vec<(usize, usize)> {
        let m = pattern.len();
        if m == 0 || m > MAX_BITAP_PATTERN || k >= 64 {
            return Vec::new();
        }
        let peq = build_peq(pattern);
        let pmask = prefix_mask(m);

        let mut r = vec![0u64; k + 1];
        for d in 1..=k {
            r[d] = (1u64 << d) - 1; // i <= d leading deletions for free
        }
        let mut ends = Vec::new();

        for (j, &tb) in text.iter().enumerate() {
            let cmask = peq[base_code(tb)];
            let mut old_prev = r[0];
            r[0] = ((r[0] << 1) | 1) & cmask;

            for d in 1..=k {
                let old_rd = r[d];
                let mut nd = ((r[d] << 1) | 1) & cmask; // exact continuation
                nd |= old_prev;                         // insertion (skip target char)
                nd |= (old_prev << 1) | 1;              // substitution
                nd |= r[d - 1] << 1;                    // deletion (skip pattern char)
                r[d] = nd & pmask;
                old_prev = old_rd;
            }

            for d in 0..=k {
                if (r[d] >> (m - 1)) & 1 == 1 {
                    ends.push((j, d));
                    break;
                }
            }
        }
        ends
    }

    /// Core bit-parallel Hamming scan (substitutions only, window == m).
    /// Returns (end_position, true_mismatch_count).
    fn hamming_ends(text: &[u8], pattern: &str, k: usize) -> Vec<(usize, usize)> {
        let m = pattern.len();
        if m == 0 || m > MAX_BITAP_PATTERN || k >= 64 {
            return Vec::new();
        }
        let peq = build_peq(pattern);
        let pmask = prefix_mask(m);

        let mut r = vec![0u64; k + 1];
        let mut ends = Vec::new();

        for (j, &tb) in text.iter().enumerate() {
            let cmask = peq[base_code(tb)];
            let not_cmask = !cmask;
            let mut old_prev = r[0];
            r[0] = ((r[0] << 1) | 1) & cmask;
            for d in 1..=k {
                let old_rd = r[d];
                let nd = (((r[d] << 1) | 1) & cmask)         // position matches
                       | (((old_prev << 1) | 1) & not_cmask); // mismatch, eats one edit
                r[d] = nd & pmask;
                old_prev = old_rd;
            }
            for d in 0..=k {
                if (r[d] >> (m - 1)) & 1 == 1 {
                    ends.push((j, d));
                    break;
                }
            }
        }
        ends
    }

    /// Fuzzy search (Levenshtein) of `pattern` in `target`, max distance k.
    /// One entry per matching END position with the true edit distance and
    /// the shortest aligned target window.
    pub fn search(target: &PackedDna, pattern: &str, max_distance: usize) -> Vec<AlignmentMatch> {
        let ascii = target.to_ascii();
        let ends = Self::levenshtein_ends(ascii.as_bytes(), pattern, max_distance);
        ends.into_iter()
            .map(|(j, d)| {
                let (start, window) = best_window(&ascii, pattern, j, d);
                AlignmentMatch {
                    offset_bp: start,
                    mismatches: d,
                    matched_sequence: window,
                }
            })
            .collect()
    }

    /// Pure Hamming search (substitutions only). Window length == pattern length.
    pub fn search_hamming(target: &PackedDna, pattern: &str, max_distance: usize) -> Vec<AlignmentMatch> {
        let ascii = target.to_ascii();
        let ends = Self::hamming_ends(ascii.as_bytes(), pattern, max_distance);
        let m = pattern.len();
        ends.into_iter()
            .map(|(j, d)| {
                let start = j + 1 - m;
                AlignmentMatch {
                    offset_bp: start,
                    mismatches: d,
                    matched_sequence: ascii[start..=j].to_string(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Deterministic xorshift RNG (no external deps)
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn pick_base(&mut self) -> u8 {
            b"ACGT"[(self.next() % 4) as usize]
        }
    }

    fn rand_seq(rng: &mut Rng, n: usize) -> String {
        (0..n).map(|_| rng.pick_base() as char).collect()
    }

    /// Reference: semi-global edit distance column.
    /// Some(min_dist) if a substring of `text` ending at its last position
    /// has edit distance <= k from `pattern` (min over all start offsets).
    fn ref_lev_ending(text: &[u8], pattern: &str, k: usize) -> Option<usize> {
        let m = pattern.len();
        let mut prev: Vec<usize> = (0..=m).collect(); // D[i][j-1], D[i][0] = i
        for &tb in text {
            let mut cur = vec![usize::MAX; m + 1];
            cur[0] = 0; // free start
            for i in 1..=m {
                let sub = prev[i - 1] + (pattern.as_bytes()[i - 1] != tb) as usize;
                let del = prev[i] + 1;
                let ins = cur[i - 1] + 1;
                cur[i] = sub.min(del).min(ins);
            }
            prev = cur;
        }
        let d = prev[m];
        if d <= k { Some(d) } else { None }
    }

    fn naive_lev_test(a: &str, b: &str) -> usize {
        naive_lev(a, b)
    }

    #[test]
    fn exact_matches_found() {
        let t = PackedDna::from_ascii("TTGACAGTCAGTCATGCAAG").unwrap();
        let mh = BitapMatcher::search_hamming(&t, "CAGTCA", 0);
        let offs: Vec<usize> = mh.iter().map(|x| x.offset_bp).collect();
        assert_eq!(offs, vec![4, 8]); // CAGTCA at 4 and 8
        assert!(mh.iter().all(|x| x.mismatches == 0));

        // Levenshtein with k=0 == exact search
        let ml = BitapMatcher::search(&t, "CAGTCA", 0);
        let offs_l: Vec<usize> = ml.iter().map(|x| x.offset_bp).collect();
        assert_eq!(offs_l, vec![4, 8]);
    }

    #[test]
    fn single_snp_detected_with_true_distance() {
        let t = PackedDna::from_ascii("AAAAACGCTAAAA").unwrap();
        let m = BitapMatcher::search(&t, "ACGTT", 1);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].mismatches, 1);
        // alignment ends at 'T' of ACGCT (index 8)
        assert_eq!(m[0].offset_bp + m[0].matched_sequence.len() - 1, 8);
        // window guarantees: lev(pattern, window) == reported distance
        assert_eq!(naive_lev_test("ACGTT", &m[0].matched_sequence), 1);
    }

    #[test]
    fn insertion_and_deletion_detected() {
        // Deletion in target: pattern has extra base vs target
        let t2 = PackedDna::from_ascii("TTACGTTT").unwrap();
        let m2 = BitapMatcher::search(&t2, "ACGTTA", 1);
        assert!(!m2.is_empty());

        // Insertion in target
        let t3 = PackedDna::from_ascii("GGACGTTGG").unwrap();
        let m3 = BitapMatcher::search(&t3, "ACGT", 1);
        assert!(m3.iter().any(|x| x.mismatches == 1));
    }

    #[test]
    fn fuzz_levenshtein_vs_reference() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let mut checked = 0usize;
        for _ in 0..4000 {
            let n = 8 + (rng.next() % 40) as usize;
            let text = rand_seq(&mut rng, n);
            let m = 1 + (rng.next() % 8) as usize;
            if m > n { continue; }
            let pattern = rand_seq(&mut rng, m);
            let k = (rng.next() % 4) as usize;

            let target = PackedDna::from_ascii(&text).unwrap();
            let got = BitapMatcher::search(&target, &pattern, k);
            let mut got_by_end: HashMap<usize, usize> = HashMap::new();
            for mt in &got {
                let end = mt.offset_bp + mt.matched_sequence.len() - 1;
                // window property: lev(pattern, window) == reported distance
                assert_eq!(naive_lev_test(&pattern, &mt.matched_sequence), mt.mismatches,
                    "window-lev text={} pattern={}", text, pattern);
                got_by_end.entry(end)
                    .and_modify(|e| { if mt.mismatches < *e { *e = mt.mismatches; } })
                    .or_insert(mt.mismatches);
            }

            for j in 0..n {
                let want = ref_lev_ending(&text.as_bytes()[..=j], &pattern, k);
                assert_eq!(got_by_end.get(&j).copied(), want,
                    "text={} pattern={} k={} end={}", text, pattern, k, j);
                checked += 1;
            }
        }
        assert!(checked > 50_000);
    }

    #[test]
    fn fuzz_hamming_vs_reference() {
        let mut rng = Rng(0xDEADBEEFCAFEBABE);
        for _ in 0..4000 {
            let n = 8 + (rng.next() % 40) as usize;
            let text = rand_seq(&mut rng, n);
            let m = 1 + (rng.next() % 8) as usize;
            if m > n { continue; }
            let pattern = rand_seq(&mut rng, m);
            let k = (rng.next() % 4) as usize;

            let target = PackedDna::from_ascii(&text).unwrap();
            let got = BitapMatcher::search_hamming(&target, &pattern, k);
            let got_set: HashMap<usize, usize> =
                got.iter().map(|x| (x.offset_bp, x.mismatches)).collect();

            for start in 0..=(n - m) {
                let dist = text.as_bytes()[start..start + m].iter()
                    .zip(pattern.as_bytes())
                    .filter(|(a, b)| a != b).count();
                let want = if dist <= k { Some(dist) } else { None };
                assert_eq!(got_set.get(&start).copied(), want,
                    "text={} pattern={} k={} start={}", text, pattern, k, start);
            }
        }
    }

    #[test]
    fn long_pattern_rejected() {
        let t = PackedDna::from_ascii("ACGTACGT").unwrap();
        let long_pat = "A".repeat(65);
        assert!(BitapMatcher::search(&t, &long_pat, 1).is_empty());
        assert!(BitapMatcher::search_hamming(&t, &long_pat, 1).is_empty());
    }

    #[test]
    fn real_world_snp_scenario() {
        // TP53-like: find variant with 1 mismatch
        let target = PackedDna::from_ascii("GACTGCCCTGCTGGGGGTCAG").unwrap();
        let matches = BitapMatcher::search_hamming(&target, "GACTGCCCC", 1);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].offset_bp, 0);
        assert_eq!(matches[0].mismatches, 1);
        assert_eq!(matches[0].matched_sequence, "GACTGCCCT");
    }
}
