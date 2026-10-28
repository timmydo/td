//! Complete search interpretation, rebuilt only from a verified immutable body.
use crate::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    jmap_request::dispatch::{collect_verified_search, SearchFields},
    limits::Limits,
    store_fs::PinnedBlob,
};
use std::time::Instant;
use td_crypto::{BulkSha256, Digest};

pub(crate) const VERSION: i32 = SearchFields::VERSION as i32;
pub(crate) const MAX_BYTES: usize = SearchFields::MAX_ENCODED;
pub(crate) struct Prepared {
    pub(crate) fields: Vec<u8>,
    pub(crate) digest: [u8; 32],
}

pub(crate) fn compatible(limits: &Limits) -> bool {
    limits.header_bytes == 256 * 1024 && limits.mime_parts == 1024 && limits.mime_depth == 32
}

pub(crate) fn digest(
    body_digest: &[u8; 32],
    body_length: u64,
    fields: &[u8],
    deadline: Instant,
) -> Option<[u8; 32]> {
    if fields.len() > MAX_BYTES {
        return None;
    }
    let mut hash = BulkSha256::try_new().ok()?;
    hash.update(b"td-mta-search-fields-v1\0").ok()?;
    hash.update(body_digest).ok()?;
    hash.update(&body_length.to_be_bytes()).ok()?;
    hash.update(&(fields.len() as u64).to_be_bytes()).ok()?;
    for chunk in fields.chunks(65536) {
        if Instant::now() >= deadline {
            return None;
        }
        hash.update(chunk).ok()?;
    }
    (Instant::now() < deadline)
        .then(|| hash.finish().ok())
        .flatten()
}

/// The administrative job supplies its original deadline. Optional extraction
/// failure leaves no complete search row, never a successful empty index.
pub(crate) fn prepare(pin: &mut PinnedBlob<'_, '_>, deadline: Instant) -> Option<Prepared> {
    use crate::ports::BlobReader;
    let clock = RuntimeClock::new();
    let mut work = Meter::new(
        clock.deadline_before(deadline).ok()?,
        Charge {
            io_bytes: 512 * 1024 * 1024,
            records: 64 * 1024 * 1024,
            output_bytes: 128 * 1024 * 1024,
            ..Charge::default()
        },
    );
    let limits = Limits {
        header_bytes: 256 * 1024,
        mime_parts: 1024,
        mime_depth: 32,
        ..Limits::default()
    };
    let fields = collect_verified_search(pin, &limits, &clock, &mut work)
        .ok()?
        .encode()
        .ok()?;
    pin.check_deadline().ok()?;
    let digest = digest(pin.digest(), pin.len(), &fields, deadline)?;
    Some(Prepared { fields, digest })
}
