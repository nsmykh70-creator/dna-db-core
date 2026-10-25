//! Bio-Plugin Registry — in-database bioinformatics functions.
//!
//! v2.1.0: replaced the fake WASM runtime (which only printed "0 memory
//! leaks") with a REAL, extensible function registry. Shipped plugins are
//! genuine implementations:
//!   - smith_waterman: local alignment with affine-free scoring (classic DP)
//!   - crispr_pam_scan: NGG / NRG PAM site detection with seed proximity
//!   - orf_finder: start->stop open reading frames
//!   - gc_windows: sliding-window GC content
//! Registration is runtime-extensible via BioPluginFn (planned: WASM modules
//! in a future release, see ROADMAP).

use parking_lot::RwLock;
use std::collections::HashMap;
use crate::dna::PackedDna;

pub type BioPluginFn = fn(&[PackedDna], &[String]) -> Result<Vec<String>, String>;

#[derive(Debug, Clone)]
pub struct BioPlugin {
    pub name: String,
    pub version: String,
    pub description: String,
    pub func: BioPluginFn,
}

pub struct BioPluginRegistry {
    plugins: RwLock<HashMap<String, BioPlugin>>,
}

impl BioPluginRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            plugins: RwLock::new(HashMap::new()),
        };
        reg.register(
            "smith_waterman",
            "1.0.0",
            "Local alignment of two sequences (classic O(nm) DP)",
            plugins::smith_waterman,
        );
        reg.register(
            "crispr_pam_scan",
            "1.0.0",
            "Scan a sequence for NGG/NRG PAM sites",
            plugins::crispr_pam_scan,
        );
        reg.register(
            "orf_finder",
            "1.0.0",
            "Find ATG..stop open reading frames (min 30 aa by default)",
            plugins::orf_finder,
        );
        reg.register(
            "gc_windows",
            "1.0.0",
            "Sliding-window GC content (window=100 by default)",
            plugins::gc_windows,
        );
        reg
    }

    pub fn register(&mut self, name: &str, version: &str, description: &str, func: BioPluginFn) {
        self.plugins.write().insert(
            name.to_string(),
            BioPlugin {
                name: name.to_string(),
                version: version.to_string(),
                description: description.to_string(),
                func,
            },
        );
    }

    pub fn list(&self) -> Vec<(String, String, String)> {
        let g = self.plugins.read();
        let mut v: Vec<_> = g
            .values()
            .map(|p| (p.name.clone(), p.version.clone(), p.description.clone()))
            .collect();
        v.sort();
        v
    }

    pub fn execute(&self, plugin_name: &str, inputs: &[PackedDna], args: &[String]) -> Result<Vec<String>, String> {
        let guard = self.plugins.read();
        let plugin = guard
            .get(plugin_name)
            .ok_or_else(|| format!("Plugin '{}' not found", plugin_name))?;
        (plugin.func)(inputs, args)
    }
}

impl Default for BioPluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub mod plugins {
    use super::*;

    /// Smith-Waterman local alignment with simple scoring:
    /// match +2, mismatch -1, gap -1. Returns score and both aligned fragments.
    pub fn smith_waterman(inputs: &[PackedDna], _args: &[String]) -> Result<Vec<String>, String> {
        if inputs.len() < 2 {
            return Err("smith_waterman requires 2 sequences".into());
        }
        let a = inputs[0].to_ascii().to_uppercase();
        let b = inputs[1].to_ascii().to_uppercase();
        let (n, m) = (a.len(), b.len());
        let (ab, bb) = (a.as_bytes(), b.as_bytes());

        let mut dp = vec![vec![0i64; m + 1]; n + 1];
        let mut best = (0i64, 0usize, 0usize); // score, i, j
        for i in 1..=n {
            for j in 1..=m {
                let s = if ab[i - 1] == bb[j - 1] { 2 } else { -1 };
                dp[i][j] = (dp[i - 1][j - 1] + s)
                    .max(dp[i - 1][j] - 1)
                    .max(dp[i][j - 1] - 1)
                    .max(0);
                if dp[i][j] > best.0 {
                    best = (dp[i][j], i, j);
                }
            }
        }

        // Traceback from best cell
        let (mut i, mut j) = (best.1, best.2);
        let mut aln_a = Vec::new();
        let mut aln_b = Vec::new();
        while i > 0 && j > 0 && dp[i][j] > 0 {
            let s = if ab[i - 1] == bb[j - 1] { 2 } else { -1 };
            if dp[i][j] == dp[i - 1][j - 1] + s {
                aln_a.push(ab[i - 1]);
                aln_b.push(bb[j - 1]);
                i -= 1;
                j -= 1;
            } else if dp[i][j] == dp[i - 1][j] - 1 {
                aln_a.push(ab[i - 1]);
                aln_b.push(b'-');
                i -= 1;
            } else {
                aln_a.push(b'-');
                aln_b.push(bb[j - 1]);
                j -= 1;
            }
        }
        aln_a.reverse();
        aln_b.reverse();
        let sa: String = aln_a.into_iter().map(|c| c as char).collect();
        let sb: String = aln_b.into_iter().map(|c| c as char).collect();

        Ok(vec![
            format!("score={}", best.0),
            format!("aln_a={}", sa),
            format!("aln_b={}", sb),
        ])
    }

    /// CRISPR PAM scan: finds NGG (SpCas9) and NRG (Cas12a-ish) sites.
    /// Reports PAM genomic offset (1-based) and the 20bp upstream protospacer
    /// window when available.
    pub fn crispr_pam_scan(inputs: &[PackedDna], args: &[String]) -> Result<Vec<String>, String> {
        if inputs.is_empty() {
            return Err("crispr_pam_scan requires 1 sequence".into());
        }
        let seq = inputs[0].to_ascii().to_uppercase();
        let bytes = seq.as_bytes();
        let strict_ngg = args.iter().any(|a| a.eq_ignore_ascii_case("ngg"));

        let mut out = Vec::new();
        for i in 0..bytes.len().saturating_sub(2) {
            let n1 = bytes[i];
            let n2 = bytes[i + 1];
            let n3 = bytes[i + 2];
            let is_ngg = n2 == b'G' && n3 == b'G';
            let is_nrg = !strict_ngg && (n2 == b'A' || n2 == b'G') && n3 == b'G';
            if is_ngg || is_nrg {
                let proto_start = i.saturating_sub(20);
                let proto = &seq[proto_start..i];
                out.push(format!(
                    "pam@{}:{}{}{} protospacer={}",
                    i + 1,
                    n1 as char,
                    n2 as char,
                    n3 as char,
                    proto
                ));
            }
        }
        if out.is_empty() {
            out.push("no_pam_sites_found".to_string());
        }
        Ok(out)
    }

    /// ORF finder: ATG .. stop codons (TAA/TAG/TGA), min length 30 aa default.
    pub fn orf_finder(inputs: &[PackedDna], args: &[String]) -> Result<Vec<String>, String> {
        if inputs.is_empty() {
            return Err("orf_finder requires 1 sequence".into());
        }
        let min_aa: usize = args
            .first()
            .and_then(|a| a.parse().ok())
            .unwrap_or(30);
        let orfs = inputs[0].orfs(min_aa);
        let out: Vec<String> = orfs
            .into_iter()
            .map(|(off, prot)| format!("orf@{}:len_aa={}:{}", off, prot.len(), prot))
            .collect();
        if out.is_empty() {
            return Ok(vec!["no_orfs_found".to_string()]);
        }
        Ok(out)
    }

    /// Sliding-window GC content.
    pub fn gc_windows(inputs: &[PackedDna], args: &[String]) -> Result<Vec<String>, String> {
        if inputs.is_empty() {
            return Err("gc_windows requires 1 sequence".into());
        }
        let seq = inputs[0].to_ascii();
        let window: usize = args.first().and_then(|a| a.parse().ok()).unwrap_or(100);
        if window == 0 || seq.len() < window {
            return Err(format!("window {} exceeds sequence length {}", window, seq.len()));
        }
        let bytes = seq.as_bytes();
        let mut out = Vec::new();
        let mut gc = bytes[..window].iter().filter(|&&c| c == b'G' || c == b'C').count();
        out.push(format!("gc@0..{}={:.1}%", window, gc as f64 * 100.0 / window as f64));
        for start in 1..=bytes.len() - window {
            let out_going = bytes[start - 1];
            let in_coming = bytes[start + window - 1];
            if out_going == b'G' || out_going == b'C' {
                gc -= 1;
            }
            if in_coming == b'G' || in_coming == b'C' {
                gc += 1;
            }
            if start % 10 == 0 || start + window == bytes.len() {
                out.push(format!(
                    "gc@{}..{}={:.1}%",
                    start,
                    start + window,
                    gc as f64 * 100.0 / window as f64
                ));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_builtins() {
        let reg = BioPluginRegistry::new();
        let names: Vec<_> = reg.list().into_iter().map(|(n, _, _)| n).collect();
        assert!(names.contains(&"smith_waterman".to_string()));
        assert!(names.contains(&"crispr_pam_scan".to_string()));
        assert!(names.contains(&"orf_finder".to_string()));
        assert!(names.contains(&"gc_windows".to_string()));
    }

    #[test]
    fn smith_waterman_exact_substring() {
        let reg = BioPluginRegistry::new();
        let a = PackedDna::from_ascii("TTGACAGTCAGTCATGCAAG").unwrap();
        let b = PackedDna::from_ascii("CAGTCA").unwrap();
        let out = reg.execute("smith_waterman", &[a, b], &[]).unwrap();
        assert_eq!(out[0], "score=12"); // 6 * 2 exact matches
        assert_eq!(out[1], "aln_a=CAGTCA");
    }

    #[test]
    fn pam_scan_finds_ngg() {
        let reg = BioPluginRegistry::new();
        // sequence with TGG at position 5 (1-based)
        let s = PackedDna::from_ascii("ACGTATGGCCCC").unwrap();
        let out = reg.execute("crispr_pam_scan", &[s], &["NGG".to_string()]).unwrap();
        assert!(out[0].starts_with("pam@6:TGG"), "got: {}", out[0]);
    }

    #[test]
    fn orf_finder_min_length() {
        let reg = BioPluginRegistry::new();
        // 15-aa ORF: ATG + 14 codons + TAA
        let mut seq = String::from("ATG");
        for _ in 0..14 {
            seq.push_str("GCT");
        }
        seq.push_str("TAA");
        let s = PackedDna::from_ascii(&seq).unwrap();
        let out = reg.execute("orf_finder", &[s.clone()], &["10".to_string()]).unwrap();
        assert!(out[0].starts_with("orf@0:"));
        // With min 30 aa -> not found
        let out2 = reg.execute("orf_finder", &[s], &["30".to_string()]).unwrap();
        assert_eq!(out2[0], "no_orfs_found");
    }

    #[test]
    fn gc_windows_slides() {
        let reg = BioPluginRegistry::new();
        let s = PackedDna::from_ascii("GGGGCCCCGGGGCCCC").unwrap();
        let out = reg.execute("gc_windows", &[s], &["8".to_string()]).unwrap();
        assert_eq!(out[0], "gc@0..8=100.0%");
        assert!(out.iter().all(|l| l.contains("=100.0%")));
    }

    #[test]
    fn unknown_plugin_errors() {
        let reg = BioPluginRegistry::new();
        let s = PackedDna::from_ascii("ACGT").unwrap();
        assert!(reg.execute("no_such_plugin", &[s], &[]).is_err());
    }
}
