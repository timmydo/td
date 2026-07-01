//! Reference scanning + NAR hashing in one streaming pass, the pinned
//! daemon's algorithm (nix/libstore/references.cc scanForReferences /
//! RefScanSink, read off the pin):
//!   - candidates are the 32-char nix-base32 HASH PARTS of the candidate
//!     store paths (basename up to the first `-`);
//!   - the NAR DUMP of the output is searched for those strings (so file
//!     contents, symlink targets and entry names are all covered), skipping
//!     ahead on any non-base32 byte exactly like the daemon's `search`;
//!   - a match may span chunk boundaries: a 32-byte tail of the previous
//!     chunk is kept and the seam re-searched;
//!   - the same pass feeds the SHA-256 NAR hash and byte count that
//!     registration records.
//! Candidate-set note: the rung passes the staged closure (which includes
//! .drv files and sources — a superset of the daemon's input-closure +
//! outputs set). Extra never-matching candidates cannot add references; a
//! match on one would surface as a references mismatch in the differential,
//! red and diagnosable, never silently dropped.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use crate::sha256::Sha256;

const REF_LEN: usize = 32;
const BASE32_CHARS: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

fn is_base32(b: u8) -> bool {
    BASE32_CHARS.contains(&b)
}

/// The hash part of a store path: basename up to the first `-`, which must
/// be exactly 32 chars (the daemon asserts the same).
fn hash_part(path: &str) -> io::Result<[u8; REF_LEN]> {
    let base = path.rsplit('/').next().unwrap_or("");
    let part = base.split('-').next().unwrap_or("");
    if part.len() == REF_LEN {
        let mut h = [0u8; REF_LEN];
        h.copy_from_slice(part.as_bytes());
        Ok(h)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad reference candidate `{path}'"),
        ))
    }
}

/// Streaming Write sink: NAR bytes in; hash, size and seen references out.
///
/// The candidate INDEX (hash part -> store path) is immutable once built; the
/// per-scan state (which candidates were `seen`, plus the running hash/size and
/// seam tail) is separate and `reset()`-able. So a closure walk over many roots
/// pays the candidate-map build ONCE and resets between paths in O(refs seen),
/// not O(candidates) — the difference between a fast and an unusable scan when
/// the candidate set is the whole live store (hundreds of thousands of paths).
pub struct Scanner {
    sha: Sha256,
    size: u64,
    /// hash part -> full store path (the immutable candidate index)
    candidates: HashMap<[u8; REF_LEN], String>,
    /// hash parts matched in the bytes scanned since `new`/`reset`
    seen: HashSet<[u8; REF_LEN]>,
    tail: Vec<u8>,
}

impl Scanner {
    pub fn new(candidate_paths: &[String]) -> io::Result<Scanner> {
        let mut candidates = HashMap::with_capacity(candidate_paths.len());
        for p in candidate_paths {
            // Duplicate hash parts cannot happen for distinct store items;
            // last-in wins harmlessly for duplicate path entries.
            candidates.insert(hash_part(p)?, p.clone());
        }
        Ok(Scanner {
            sha: Sha256::new(),
            size: 0,
            candidates,
            seen: HashSet::new(),
            tail: Vec::new(),
        })
    }

    /// Clear the per-scan state (seen refs, hash, size, seam tail) while KEEPING
    /// the built candidate index, so the next `write_nar` scans a fresh path
    /// against the same candidates without rebuilding the map.
    pub fn reset(&mut self) {
        self.seen.clear();
        self.sha = Sha256::new();
        self.size = 0;
        self.tail.clear();
    }

    /// The daemon's `search`: backwards base32 check with skip-ahead.
    fn search(&mut self, s: &[u8]) {
        let mut i = 0;
        while i + REF_LEN <= s.len() {
            let mut skip = None;
            for j in (0..REF_LEN).rev() {
                if !is_base32(s[i + j]) {
                    skip = Some(j + 1);
                    break;
                }
            }
            if let Some(n) = skip {
                i += n;
                continue;
            }
            let window: &[u8; REF_LEN] = s[i..i + REF_LEN].try_into().unwrap();
            if self.candidates.contains_key(window) {
                self.seen.insert(*window);
            }
            i += 1;
        }
    }

    /// Sorted store paths whose hash part was matched since the last
    /// `new`/`reset` (does not consume the scanner, so the index is reusable).
    pub fn refs(&self) -> Vec<String> {
        let mut refs: Vec<String> = self
            .seen
            .iter()
            .filter_map(|h| self.candidates.get(h).cloned())
            .collect();
        refs.sort();
        refs
    }

    /// (nar sha256, nar size, sorted seen reference paths)
    pub fn finish(self) -> (String, u64, Vec<String>) {
        let refs = self.refs();
        let size = self.size;
        (
            format!("sha256:{}", crate::sha256::to_base16(&self.sha.finalize())),
            size,
            refs,
        )
    }
}

impl Write for Scanner {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sha.update(buf);
        self.size += buf.len() as u64;
        // Search the seam (previous tail + the head of this chunk), then the
        // chunk itself — the daemon's exact coverage; double-hits are idempotent.
        let head_len = buf.len().min(REF_LEN);
        let mut seam = self.tail.clone();
        seam.extend_from_slice(&buf[..head_len]);
        self.search(&seam);
        self.search(buf);
        // New tail: last (REF_LEN - head_len) bytes of the old tail, then the
        // last head_len bytes of the chunk — RefScanSink's exact arithmetic.
        let keep = REF_LEN - head_len;
        let start = self.tail.len().saturating_sub(keep);
        let mut tail = self.tail[start..].to_vec();
        tail.extend_from_slice(&buf[buf.len() - head_len..]);
        self.tail = tail;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "0123456789abcdfghijklmnpqrsvwxyz";

    fn path(hash: &str, name: &str) -> String {
        format!("/gnu/store/{hash}-{name}")
    }

    fn scan_chunks(candidates: &[String], chunks: &[&[u8]]) -> Vec<String> {
        let mut s = Scanner::new(candidates).unwrap();
        for c in chunks {
            s.write_all(c).unwrap();
        }
        s.finish().2
    }

    #[test]
    fn finds_a_contained_reference() {
        let cand = vec![path(HASH_A, "dep"), path(HASH_B, "other")];
        let data = format!("prefix {} suffix", path(HASH_A, "dep"));
        assert_eq!(scan_chunks(&cand, &[data.as_bytes()]), vec![path(HASH_A, "dep")]);
    }

    #[test]
    fn reset_reuses_the_index_with_no_bleed_through() {
        // The invariant store-closure-scan relies on: build the candidate index
        // ONCE, then reset() between roots — each scan sees only its OWN path's
        // refs (no bleed-through from a prior root) and matches a fresh scanner.
        let cand = vec![path(HASH_A, "dep"), path(HASH_B, "other")];
        let d1 = format!("uses {}", path(HASH_A, "dep"));
        let d2 = format!("uses {}", path(HASH_B, "other"));

        let mut reused = Scanner::new(&cand).unwrap();
        reused.write_all(d1.as_bytes()).unwrap();
        assert_eq!(reused.refs(), vec![path(HASH_A, "dep")]);
        // Reset, then a DIFFERENT path — the first path's ref must NOT persist.
        reused.reset();
        reused.write_all(d2.as_bytes()).unwrap();
        assert_eq!(reused.refs(), vec![path(HASH_B, "other")]);
        // The reset scanner's finish() tuple equals a fresh scanner's for d2.
        assert_eq!(reused.finish().2, scan_chunks(&cand, &[d2.as_bytes()]));
    }

    #[test]
    fn finds_a_boundary_spanning_reference() {
        // Split the hash across two writes: the seam logic must see it.
        let cand = vec![path(HASH_B, "dep")];
        let data = format!("xx{}yy", HASH_B);
        let (a, b) = data.as_bytes().split_at(18);
        assert_eq!(scan_chunks(&cand, &[a, b]), vec![path(HASH_B, "dep")]);
        // ... wherever the split lands.
        for split in 1..data.len() {
            let (a, b) = data.as_bytes().split_at(split);
            assert_eq!(scan_chunks(&cand, &[a, b]).len(), 1, "split at {split}");
        }
    }

    #[test]
    fn ignores_non_candidates_and_broken_hashes() {
        let cand = vec![path(HASH_A, "dep")];
        // A base32 window that is not a candidate.
        assert!(scan_chunks(&cand, &[HASH_B.as_bytes()]).is_empty());
        // The candidate's hash with one non-base32 char inside ('e').
        let broken = format!("{}e{}", &HASH_A[..16], &HASH_A[17..]);
        assert!(scan_chunks(&cand, &[broken.as_bytes()]).is_empty());
    }

    #[test]
    fn hash_and_size_match_plain_hashing() {
        // The scanner must not perturb the hash/size it reports.
        let mut s = Scanner::new(&[]).unwrap();
        s.write_all(b"hello world").unwrap();
        let (h, n, refs) = s.finish();
        let mut plain = Sha256::new();
        plain.update(b"hello world");
        assert_eq!(h, format!("sha256:{}", crate::sha256::to_base16(&plain.finalize())));
        assert_eq!(n, 11);
        assert!(refs.is_empty());
    }

    #[test]
    fn rejects_malformed_candidates() {
        assert!(Scanner::new(&["/gnu/store/short-x".to_string()]).is_err());
    }
}
