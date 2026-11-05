use crate::{invalid, sha256, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::ops::Range;
use std::time::Duration;

#[cfg(test)]
use crate::image_chunks::{gear, MAX, MIN};
use crate::image_chunks::{Chunker, TARGET};
const MAX_RECORDS: usize = 1_000_000;

#[derive(Clone, Copy)]
pub enum Method {
    Fixed,
    Cdc,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Cdc => "cdc",
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    hash: [u8; 32],
    size: usize,
}

pub struct Object {
    key: Key,
    range: Range<usize>,
}

fn key(bytes: &[u8]) -> Key {
    let mut hash = sha256::Sha256::new();
    hash.update(bytes);
    Key {
        hash: hash.finalize(),
        size: bytes.len(),
    }
}

pub fn index(bytes: &[u8], method: Method) -> Result<Vec<Object>> {
    let mut spans = Vec::new();
    let mut start = 0;
    let mut chunker = Chunker::default();
    for (offset, byte) in bytes.iter().enumerate() {
        let length = offset + 1 - start;
        let cut = match method {
            Method::Fixed => length == TARGET,
            Method::Cdc => chunker.push(*byte),
        };
        if cut {
            spans.push(start..offset + 1);
            start = offset + 1;
        }
    }
    if start < bytes.len() {
        spans.push(start..bytes.len());
    }
    ranges(bytes, &spans)
}

pub fn ranges(bytes: &[u8], spans: &[Range<usize>]) -> Result<Vec<Object>> {
    if spans.len() > MAX_RECORDS {
        return Err(invalid("too many index records").into());
    }
    let mut end = 0;
    let mut result = Vec::new();
    for span in spans {
        if span.start != end || span.end <= span.start {
            return Err(invalid("index is not an exact ordered partition").into());
        }
        let data = bytes
            .get(span.clone())
            .ok_or_else(|| invalid("index exceeds payload"))?;
        result.push(Object {
            key: key(data),
            range: span.clone(),
        });
        end = span.end;
    }
    if end != bytes.len() {
        return Err(invalid("index leaves uncovered bytes").into());
    }
    Ok(result)
}

pub struct Stats {
    records: usize,
    unique: usize,
    cold: u64,
    missing: usize,
    missing_bytes: u64,
    reused_output_bytes: u64,
}

impl Stats {
    pub fn print(&self, payload: &str, method: &str, indexing: Duration, reconstruction: Duration) {
        println!("transfer\t{payload}\t{method}\trecords={}\tunique_objects={}\tcold_object_bytes={}\tmissing_objects={}\tmissing_object_bytes={}\treused_output_bytes={}\tindex_model_bytes={}\tindex_ms={}\treconstruct_sync_ms={}",
            self.records, self.unique, self.cold, self.missing, self.missing_bytes,
            self.reused_output_bytes, 48 + self.records as u64 * 48,
            indexing.as_millis(), reconstruction.as_millis());
    }
}

pub fn reconstruct(
    a: &[u8],
    b: &[u8],
    old: &[Object],
    new: &[Object],
    writer: &mut impl Write,
) -> Result<Stats> {
    let cache: BTreeMap<_, _> = old.iter().map(|o| (&o.key, &o.range)).collect();
    let mut seen = BTreeSet::new();
    let mut stats = Stats {
        records: new.len(),
        unique: 0,
        cold: 0,
        missing: 0,
        missing_bytes: 0,
        reused_output_bytes: 0,
    };
    let mut hash = sha256::Sha256::new();
    let mut length = 0;
    for object in new {
        let cached = cache.get(&object.key);
        let data = match cached {
            Some(span) => a.get((*span).clone()),
            None => b.get(object.range.clone()),
        }
        .ok_or_else(|| invalid("object range outside payload"))?;
        if key(data) != object.key {
            return Err(invalid("object hash mismatch").into());
        }
        if seen.insert(&object.key) {
            stats.unique += 1;
            stats.cold += data.len() as u64;
            if cached.is_none() {
                stats.missing += 1;
                stats.missing_bytes += data.len() as u64;
            }
        }
        if cached.is_some() {
            stats.reused_output_bytes += data.len() as u64;
        }
        writer.write_all(data)?;
        hash.update(data);
        length += data.len();
    }
    if length != b.len() || hash.finalize() != key(b).hash {
        return Err(invalid("reconstructed payload digest mismatch").into());
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn random(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (gear((i % 251) as u8) ^ gear((i / 251 % 253) as u8)) as u8)
            .collect()
    }

    #[test]
    fn cdc_recovers_after_a_shift_and_reconstructs_exactly() {
        let a = random(4 * 1024 * 1024);
        let mut b = b"inserted prefix".to_vec();
        b.extend_from_slice(&a);
        let old = index(&a, Method::Cdc).unwrap();
        let new = index(&b, Method::Cdc).unwrap();
        let mut output = Vec::new();
        let stats = reconstruct(&a, &b, &old, &new, &mut output).unwrap();
        assert_eq!(output, b);
        assert!(stats.reused_output_bytes > (a.len() * 3 / 4) as u64);
    }

    #[test]
    fn duplicate_missing_objects_count_once_and_corruption_refuses() {
        let a = vec![1; TARGET];
        let b = vec![2; TARGET * 3];
        let old = index(&a, Method::Fixed).unwrap();
        let new = index(&b, Method::Fixed).unwrap();
        let stats = reconstruct(&a, &b, &old, &new, &mut Vec::new()).unwrap();
        assert_eq!(stats.missing, 1);
        assert_eq!(stats.missing_bytes, TARGET as u64);
        let mut corrupt = a.clone();
        corrupt[0] ^= 1;
        assert!(reconstruct(&corrupt, &a, &old, &old, &mut Vec::new()).is_err());
        let mut corrupt = b.clone();
        corrupt[0] ^= 1;
        assert!(reconstruct(&a, &corrupt, &old, &new, &mut Vec::new()).is_err());
    }

    #[test]
    fn partitions_cover_tails_empty_and_zero_filled_data() {
        for n in [0, 1, MIN, MAX, MAX + 1] {
            let bytes = vec![0; n];
            for method in [Method::Fixed, Method::Cdc] {
                let objects = index(&bytes, method).unwrap();
                let mut output = Vec::new();
                reconstruct(&[], &bytes, &[], &objects, &mut output).unwrap();
                assert_eq!(output, bytes);
                assert!(objects.iter().all(|o| o.key.size <= MAX));
            }
        }
        assert!(ranges(&[0; 10], &[0..4, 5..10]).is_err());
        assert!(ranges(&[0; 10], &[0..4, 3..10]).is_err());
        assert!(ranges(&[0; 10], std::slice::from_ref(&(0..11))).is_err());
    }
}
