//! Worker-local projections of previously verified immutable headers.
use crate::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_body::Metadata,
};

const MAX_ENTRIES: usize = 256;
const MAX_FRAGMENT: usize = 64 * 1024;
#[derive(Clone, Copy, Eq, PartialEq)]
struct Key {
    account: AccountId,
    epoch: StoreEpoch,
    blob: BlobId,
    length: u64,
    digest: [u8; 32],
}
impl From<Metadata> for Key {
    fn from(m: Metadata) -> Self {
        Self {
            account: m.identity.account,
            epoch: m.identity.epoch,
            blob: m.blob,
            length: m.body.length,
            digest: m.body.digest,
        }
    }
}
pub(crate) struct Capture {
    key: Key,
    signature: Vec<u8>,
    bytes: Vec<u8>,
    committed: bool,
}
impl Capture {
    fn allocation(&self) -> usize {
        self.signature
            .capacity()
            .saturating_add(self.bytes.capacity())
    }
}
pub(crate) struct Cache {
    maximum: usize,
    entries: Vec<Capture>,
}
impl Cache {
    pub(crate) fn new(maximum: usize) -> Self {
        Self {
            maximum,
            entries: Vec::new(),
        }
    }
    fn allocation(&self) -> usize {
        self.entries
            .capacity()
            .saturating_mul(std::mem::size_of::<Capture>())
            .saturating_add(self.entries.iter().map(Capture::allocation).sum::<usize>())
    }
    fn room(&mut self, additional: usize) -> bool {
        while self.allocation().saturating_add(additional) > self.maximum {
            if self.entries.is_empty() {
                return false;
            }
            self.entries.remove(0);
        }
        true
    }
    pub(crate) fn transaction(&mut self) -> Transaction<'_> {
        self.entries.retain(|entry| entry.committed);
        Transaction { cache: self }
    }
    pub(crate) fn get<'a>(
        &self,
        metadata: Metadata,
        names: impl Iterator<Item = &'a [u8]> + Clone,
    ) -> Option<&[u8]> {
        let key = Key::from(metadata);
        // Names are bounded validated protocol properties; length framing is exact.
        self.entries
            .iter()
            .find(|entry| {
                entry.committed
                    && entry.key == key
                    && signature_matches(&entry.signature, names.clone())
            })
            .map(|entry| entry.bytes.as_slice())
    }
    pub(crate) fn begin<'a>(
        &mut self,
        metadata: Metadata,
        names: impl Iterator<Item = &'a [u8]> + Clone,
    ) -> Option<Capture> {
        if self.entries.capacity() == 0 {
            let bytes = MAX_ENTRIES.checked_mul(std::mem::size_of::<Capture>())?;
            if bytes > self.maximum || self.entries.try_reserve_exact(MAX_ENTRIES).is_err() {
                return None;
            }
            if self.allocation() > self.maximum {
                self.entries = Vec::new();
                return None;
            }
        }
        if self.entries.len() == MAX_ENTRIES {
            self.entries.remove(0);
        }
        let length = names.clone().try_fold(0usize, |sum, name| {
            sum.checked_add(4)?.checked_add(name.len())
        })?;
        if !self.room(length) {
            return None;
        }
        let mut signature = Vec::new();
        signature.try_reserve_exact(length).ok()?;
        if !self.room(signature.capacity()) {
            return None;
        }
        for name in names {
            signature.extend_from_slice(&u32::try_from(name.len()).ok()?.to_be_bytes());
            signature.extend_from_slice(name);
        }
        Some(Capture {
            key: metadata.into(),
            signature,
            bytes: Vec::new(),
            committed: false,
        })
    }
    pub(crate) fn append(&mut self, capture: &mut Option<Capture>, bytes: &[u8]) {
        let Some(entry) = capture.as_mut() else {
            return;
        };
        let Some(length) = entry
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_FRAGMENT)
        else {
            *capture = None;
            return;
        };
        if length > entry.bytes.capacity() {
            let capacity = length.next_power_of_two().max(256);
            // Include old and replacement allocations while copying; no realloc peak escapes admission.
            if !self.room(entry.allocation().saturating_add(capacity)) {
                *capture = None;
                return;
            }
            let mut replacement = Vec::new();
            if replacement.try_reserve_exact(capacity).is_err()
                || !self.room(entry.allocation().saturating_add(replacement.capacity()))
            {
                *capture = None;
                return;
            }
            replacement.extend_from_slice(&entry.bytes);
            entry.bytes = replacement;
        }
        entry.bytes.extend_from_slice(bytes);
    }
    pub(crate) fn stage(&mut self, capture: Option<Capture>) {
        if let Some(entry) = capture {
            if self.entries.len() < self.entries.capacity() && self.room(entry.allocation()) {
                self.entries.push(entry);
            }
        }
    }
}
fn signature_matches<'a>(mut signature: &[u8], names: impl Iterator<Item = &'a [u8]>) -> bool {
    for name in names {
        let Ok(length) = u32::try_from(name.len()) else {
            return false;
        };
        let Some(prefix) = signature.get(..4) else {
            return false;
        };
        if prefix != length.to_be_bytes() {
            return false;
        }
        let Some(rest) = signature.get(4..) else {
            return false;
        };
        if rest.get(..name.len()) != Some(name) {
            return false;
        }
        let Some(rest) = rest.get(name.len()..) else {
            return false;
        };
        signature = rest;
    }
    signature.is_empty()
}
pub(crate) struct Transaction<'a> {
    pub(crate) cache: &'a mut Cache,
}
impl Transaction<'_> {
    pub(crate) fn commit(self) {
        for entry in &mut self.cache.entries {
            entry.committed = true;
        }
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        self.cache.entries.retain(|entry| entry.committed);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    fn metadata() -> Metadata {
        Metadata {
            identity: crate::ports::ViewIdentity {
                account: AccountId::from_bytes([1; 16]),
                epoch: StoreEpoch::from_bytes([2; 16]),
                committed_sequence: crate::format::Sequence::from_u64(3),
                history_floor: crate::format::Sequence::from_u64(0),
            },
            email: crate::ids::EmailId::from_bytes([3; 16]),
            blob: BlobId::from_bytes([4; 16]),
            thread: crate::ids::ThreadId::from_bytes([5; 16]),
            received_at: 0,
            body: crate::format::row::BlobRow {
                length: 100,
                digest: [6; 32],
                created_at: 0,
            },
        }
    }
    const NAMES: &[&[u8]] = &[b"subject", b"header:X-Test:asText"];
    fn insert(cache: &mut Cache, metadata: Metadata, bytes: &[u8]) {
        let mut capture = cache.begin(metadata, NAMES.iter().copied());
        assert!(capture.is_some());
        cache.append(&mut capture, bytes);
        cache.stage(capture);
    }
    #[test]
    fn only_successful_transaction_exposes_exact_scoped_projection() {
        let mut cache = Cache::new(128 * 1024);
        let m = metadata();
        {
            let tx = cache.transaction();
            insert(tx.cache, m, b",\"subject\":\"test\"");
            assert!(tx.cache.get(m, NAMES.iter().copied()).is_none());
        }
        assert!(cache.get(m, NAMES.iter().copied()).is_none());
        let tx = cache.transaction();
        insert(tx.cache, m, b",\"subject\":\"test\"");
        tx.commit();
        assert_eq!(
            cache.get(m, NAMES.iter().copied()),
            Some(b",\"subject\":\"test\"".as_slice())
        );
        for changed in [
            Metadata {
                blob: BlobId::from_bytes([7; 16]),
                ..m
            },
            Metadata {
                body: crate::format::row::BlobRow {
                    length: 101,
                    ..m.body
                },
                ..m
            },
            Metadata {
                body: crate::format::row::BlobRow {
                    digest: [7; 32],
                    ..m.body
                },
                ..m
            },
            Metadata {
                identity: crate::ports::ViewIdentity {
                    account: AccountId::from_bytes([7; 16]),
                    ..m.identity
                },
                ..m
            },
            Metadata {
                identity: crate::ports::ViewIdentity {
                    epoch: StoreEpoch::from_bytes([7; 16]),
                    ..m.identity
                },
                ..m
            },
        ] {
            assert!(cache.get(changed, NAMES.iter().copied()).is_none());
        }
        assert!(cache.get(m, NAMES.iter().rev().copied()).is_none());
        assert!(cache
            .get(m, [b"Subject".as_slice(), NAMES[1]].into_iter())
            .is_none());
        assert!(cache.get(m, NAMES[..1].iter().copied()).is_none());
        let changed = Metadata {
            identity: crate::ports::ViewIdentity {
                committed_sequence: crate::format::Sequence::from_u64(99),
                ..m.identity
            },
            ..m
        };
        assert!(cache.get(changed, NAMES.iter().copied()).is_some());
    }
    #[test]
    fn bounded_capture_eviction_and_disabled_cache_are_optional() {
        let mut tiny = Cache::new(1);
        assert!(tiny.begin(metadata(), NAMES.iter().copied()).is_none());
        let mut cache = Cache::new(128 * 1024);
        for index in 0..512u16 {
            let mut m = metadata();
            let mut id = [0; 16];
            id[..2].copy_from_slice(&index.to_be_bytes());
            m.blob = BlobId::from_bytes(id);
            let tx = cache.transaction();
            insert(tx.cache, m, &[b'a'; 513]);
            assert!(tx.cache.allocation() <= tx.cache.maximum);
            assert!(tx.cache.entries.len() <= MAX_ENTRIES);
            tx.commit();
        }
        let mut capture = cache.begin(metadata(), NAMES.iter().copied());
        for _ in 0..16 {
            cache.append(&mut capture, &[b'b'; 4096]);
            assert!(
                cache.allocation() + capture.as_ref().map_or(0, Capture::allocation)
                    <= cache.maximum
            );
        }
        cache.append(&mut capture, b"x");
        assert!(capture.is_none());
    }
}
