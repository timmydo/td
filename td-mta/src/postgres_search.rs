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
    pub(crate) candidate_text: String,
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
    let candidate_text = candidate_text(&fields, deadline)?;
    Some(Prepared {
        fields,
        candidate_text,
        digest,
    })
}

// Keep field boundaries and replace PostgreSQL text's forbidden NUL. Operands
// containing NUL are never used as index filters; the matcher sees original fields.
fn candidate_text(encoded: &[u8], deadline: Instant) -> Option<String> {
    if encoded.len() > MAX_BYTES {
        return None;
    }
    let mut text = Vec::new();
    text.try_reserve_exact(encoded.len()).ok()?;
    let mut rest = encoded;
    let mut count = 0usize;
    while !rest.is_empty() {
        count = count.checked_add(1)?;
        if count > SearchFields::MAX_FIELDS || Instant::now() >= deadline {
            return None;
        }
        let size = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        rest = rest.get(4..)?;
        let field = rest.get(..size)?;
        std::str::from_utf8(field).ok()?;
        rest = rest.get(size..)?;
        for chunk in field.chunks(65536) {
            if Instant::now() >= deadline {
                return None;
            }
            text.extend(
                chunk
                    .iter()
                    .map(|byte| if *byte == 0 { b'\n' } else { *byte }),
            );
        }
        text.push(b'\n');
    }
    if text.len() > SearchFields::MAX_BYTES + SearchFields::MAX_FIELDS || Instant::now() >= deadline
    {
        return None;
    }
    String::from_utf8(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn encoded(fields: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for field in fields {
            bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
            bytes.extend_from_slice(field.as_bytes());
        }
        bytes
    }
    #[test]
    fn candidate_text_preserves_unicode_and_separates_fields_and_nul() {
        let deadline = Instant::now() + std::time::Duration::from_secs(3600);
        assert_eq!(
            candidate_text(&encoded(&["αβγ straße", "100%_\\", "nul\0value"]), deadline),
            Some("αβγ straße\n100%_\\\nnul\nvalue\n".into())
        );
        assert_eq!(candidate_text(&[], deadline), Some(String::new()));
    }
    #[test]
    fn candidate_text_refuses_bad_framing_utf8_and_expiry() {
        let deadline = Instant::now() + std::time::Duration::from_secs(3600);
        for bytes in [
            &[0, 0, 0][..],
            &[0, 0, 0, 2, b'a'][..],
            &[0, 0, 0, 1, 255][..],
        ] {
            assert!(candidate_text(bytes, deadline).is_none());
        }
        assert!(candidate_text(&encoded(&["x"]), Instant::now()).is_none());
    }
}
