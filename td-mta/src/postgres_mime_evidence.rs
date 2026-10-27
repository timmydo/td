//! Versioned, rebuildable MIME evidence from publication-owned message bytes.
use crate::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    header_select::SourceEnd,
    mime_structure::{self, Encoding, Media, Part},
    nfc::HeaderBudget,
    ports::Clock,
};
use std::time::Instant;
use td_crypto::{BulkSha256, Crypto, Digest, Provider};

pub(crate) const VERSION: i32 = 1;
pub(crate) const HEADER_BYTES: usize = 256 * 1024;
pub(crate) const PARTS: usize = 1024;
pub(crate) const DEPTH: usize = 32;
const PART_BYTES: usize = 56;
const CHUNK: usize = 65536;

#[derive(Clone)]
pub(crate) struct Prepared {
    pub(crate) table: Vec<u8>,
    pub(crate) chunks: Vec<u8>,
    pub(crate) header_bytes: u64,
    pub(crate) digest: [u8; 32],
}

pub(crate) struct Evidence {
    pub(crate) parts: Vec<Part>,
    pub(crate) header_bytes: u64,
}

pub(crate) fn limits() -> mime_structure::Limits {
    mime_structure::Limits {
        header_bytes: HEADER_BYTES,
        mime_depth: DEPTH,
        mime_parts: PARTS,
    }
}

fn backing<T: Default + Clone>(length: usize) -> Option<Vec<T>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).ok()?;
    bytes.resize(length, T::default());
    Some(bytes)
}

fn meter(clock: &RuntimeClock, deadline: Instant) -> Option<Meter> {
    Some(Meter::new(
        clock.deadline_before(deadline).ok()?,
        Charge {
            io_bytes: 512 * 1024 * 1024,
            records: 64 * 1024 * 1024,
            output_bytes: 128 * 1024 * 1024,
            ..Charge::default()
        },
    ))
}

/// Standalone administrative work; metered publication uses `prepare_metered`.
pub(crate) fn prepare(bytes: &[u8], body_digest: &[u8; 32], deadline: Instant) -> Option<Prepared> {
    let clock = RuntimeClock::new();
    let mut work = meter(&clock, deadline)?;
    prepare_using(bytes, body_digest, deadline, &mut work, &clock)
}

/// Prepay optional cache work from the original job without exhausting credit
/// needed by publication. The child cannot spend more or refund failed work.
pub(crate) fn prepare_metered(
    bytes: &[u8],
    body_digest: &[u8; 32],
    deadline: Instant,
    original: &mut Meter,
    original_clock: &dyn Clock,
) -> Option<Prepared> {
    let length = bytes.len() as u64;
    let left = original.remaining();
    let credit = Charge {
        io_bytes: (left.io_bytes / 2).min(length.saturating_mul(36).saturating_add(256 * 1024)),
        records: (left.records / 2).min(length.saturating_mul(4).saturating_add(4096)),
        output_bytes: (left.output_bytes / 2)
            .min(length.saturating_mul(2).saturating_add(128 * 1024)),
        ..Charge::default()
    };
    if credit.io_bytes == 0 || credit.records == 0 || credit.output_bytes == 0 {
        return None;
    }
    original
        .charge(original_clock.monotonic().ok()?, credit)
        .ok()?;
    let clock = RuntimeClock::new();
    let mut work = Meter::new(clock.deadline_before(deadline).ok()?, credit);
    prepare_using(bytes, body_digest, deadline, &mut work, &clock)
}

fn evidence_charge(work: &mut Meter, clock: &dyn Clock, parts: usize, chunks: usize) -> Option<()> {
    let table = parts.checked_mul(PART_BYTES)? as u64;
    let hashes = chunks.checked_mul(32)? as u64;
    work.charge(
        clock.monotonic().ok()?,
        Charge {
            io_bytes: table * 2 + hashes + 128,
            records: (parts + chunks + 1) as u64,
            output_bytes: table + hashes + 32,
            ..Charge::default()
        },
    )
    .ok()
}

fn prepare_using(
    bytes: &[u8],
    body_digest: &[u8; 32],
    deadline: Instant,
    work: &mut Meter,
    clock: &dyn Clock,
) -> Option<Prepared> {
    if bytes.len() as u64 > crate::postgres_body::MAX_BODY_BYTES {
        return None;
    }
    let mut budget = HeaderBudget::new();
    let mut parts = Vec::new();
    parts.try_reserve_exact(PARTS).ok()?;
    parts.resize(PARTS, Part::default());
    let mut cursor = mime_structure::Cursor::new(
        bytes,
        0,
        SourceEnd::Eof,
        limits(),
        &mut parts,
        work,
        &mut budget,
    )
    .ok()?;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        if cursor.poll(clock.monotonic().ok()?).ok()? == mime_structure::Status::Complete {
            break;
        }
    }
    let header_bytes = cursor.header_bytes()?;
    let (parts, work, _) = cursor.finish(clock.monotonic().ok()?).ok()?;
    evidence_charge(work, clock, parts.len(), bytes.len().div_ceil(CHUNK))?;
    let table = encode(parts)?;
    let mut chunks = Vec::new();
    chunks
        .try_reserve_exact(bytes.len().div_ceil(CHUNK).checked_mul(32)?)
        .ok()?;
    for chunk in bytes.chunks(CHUNK) {
        if Instant::now() >= deadline {
            return None;
        }
        work.charge(
            clock.monotonic().ok()?,
            Charge {
                io_bytes: chunk.len() as u64,
                records: 1,
                ..Charge::default()
            },
        )
        .ok()?;
        let mut hash = BulkSha256::try_new().ok()?;
        hash.update(chunk).ok()?;
        chunks.extend_from_slice(&hash.finish().ok()?);
    }
    let digest = manifest_digest(
        bytes.len() as u64,
        body_digest,
        header_bytes,
        &table,
        &chunks,
    )?;
    (Instant::now() < deadline).then_some(Prepared {
        table,
        chunks,
        header_bytes,
        digest,
    })
}

/// Rebuild without allocating a message-sized buffer. The caller has already
/// verified the whole body and retains the same snapshot for all rereads.
pub(crate) fn prepare_verified(
    pin: &mut crate::store_fs::PinnedBlob<'_, '_>,
    body_digest: &[u8; 32],
    deadline: Instant,
) -> Result<Option<Prepared>, crate::ports::Error> {
    use crate::ports::BlobReader;
    let result = (|| {
        let clock = RuntimeClock::new();
        let mut work = meter(&clock, deadline)?;
        let mut budget = HeaderBudget::new();
        let mut arena = backing(HEADER_BYTES + PARTS + 2)?;
        let mut parts = backing(PARTS)?;
        let mut headers = backing(PARTS)?;
        let mut input = [0; 4096];
        let length = pin.len();
        let mut cursor = crate::blob_structure::Cursor::new(
            pin,
            crate::blob_structure::Settings {
                extent: 0..length,
                limits: limits(),
            },
            crate::blob_structure::Buffers {
                header_bytes: &mut arena,
                body: &mut input,
                parts: &mut parts,
                headers: &mut headers,
            },
            &mut work,
            &mut budget,
            clock.monotonic().ok()?,
        )
        .ok()?;
        while cursor.poll_batch(&clock).ok()? != mime_structure::Status::Complete {}
        let retained = cursor.finish(clock.monotonic().ok()?).ok()?;
        evidence_charge(
            retained.work,
            &clock,
            retained.parts.len(),
            usize::try_from(length.div_ceil(CHUNK as u64)).ok()?,
        )?;
        let table = encode(retained.parts)?;
        let header_bytes = retained.header_bytes;
        let mut chunks = Vec::new();
        chunks
            .try_reserve_exact(usize::try_from(length.div_ceil(CHUNK as u64) * 32).ok()?)
            .ok()?;
        let mut buffer = backing(CHUNK)?;
        let mut offset = 0;
        while offset < length {
            let count = usize::try_from((length - offset).min(CHUNK as u64)).ok()?;
            retained
                .work
                .charge(
                    clock.monotonic().ok()?,
                    Charge {
                        io_bytes: count as u64 * 2,
                        records: 1,
                        ..Charge::default()
                    },
                )
                .ok()?;
            let bytes = buffer.get_mut(..count)?;
            let mut read = 0;
            while read < count {
                let n = retained
                    .source
                    .read_at(offset + read as u64, bytes.get_mut(read..)?)
                    .ok()?;
                if n == 0 {
                    return None;
                }
                read = read.checked_add(n).filter(|n| *n <= count)?;
            }
            let mut hash = BulkSha256::try_new().ok()?;
            hash.update(bytes).ok()?;
            chunks.extend_from_slice(&hash.finish().ok()?);
            offset += count as u64;
        }
        let digest = manifest_digest(length, body_digest, header_bytes, &table, &chunks)?;
        Some(Prepared {
            table,
            chunks,
            header_bytes,
            digest,
        })
    })();
    pin.check_deadline()?;
    Ok(result)
}

fn encode(parts: &[Part]) -> Option<Vec<u8>> {
    if parts.is_empty() || parts.len() > PARTS {
        return None;
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(parts.len().checked_mul(PART_BYTES)?)
        .ok()?;
    for part in parts {
        for value in [
            part.entity_start,
            part.entity_end,
            part.body_start,
            part.size,
            part.type_start,
            part.type_end,
        ] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(&part.ordinal.to_be_bytes());
        bytes.extend_from_slice(&part.parent.to_be_bytes());
        bytes.push(part.depth);
        bytes.push(match part.media {
            Media::Other => 0,
            Media::TextPlain => 1,
            Media::MessageRfc822 => 2,
            Media::Multipart => 3,
        });
        bytes.push(match part.encoding {
            Encoding::Identity => 0,
            Encoding::Base64 => 1,
            Encoding::QuotedPrintable => 2,
        });
        bytes.push(part.diagnostics);
    }
    Some(bytes)
}

fn take<'a, const N: usize>(bytes: &mut &'a [u8]) -> Option<&'a [u8; N]> {
    let (head, tail) = bytes.split_at_checked(N)?;
    *bytes = tail;
    head.try_into().ok()
}

fn decode(mut bytes: &[u8]) -> Option<Part> {
    let part = Part {
        entity_start: u64::from_be_bytes(*take(&mut bytes)?),
        entity_end: u64::from_be_bytes(*take(&mut bytes)?),
        body_start: u64::from_be_bytes(*take(&mut bytes)?),
        size: u64::from_be_bytes(*take(&mut bytes)?),
        type_start: u64::from_be_bytes(*take(&mut bytes)?),
        type_end: u64::from_be_bytes(*take(&mut bytes)?),
        ordinal: u16::from_be_bytes(*take(&mut bytes)?),
        parent: u16::from_be_bytes(*take(&mut bytes)?),
        depth: *take::<1>(&mut bytes)?.first()?,
        media: match take::<1>(&mut bytes)?.first()? {
            0 => Media::Other,
            1 => Media::TextPlain,
            2 => Media::MessageRfc822,
            3 => Media::Multipart,
            _ => return None,
        },
        encoding: match take::<1>(&mut bytes)?.first()? {
            0 => Encoding::Identity,
            1 => Encoding::Base64,
            2 => Encoding::QuotedPrintable,
            _ => return None,
        },
        diagnostics: *take::<1>(&mut bytes)?.first()?,
    };
    bytes.is_empty().then_some(part)
}

fn manifest_digest(
    length: u64,
    body_digest: &[u8; 32],
    header_bytes: u64,
    table: &[u8],
    chunks: &[u8],
) -> Option<[u8; 32]> {
    let mut hash = BulkSha256::try_new().ok()?;
    for bytes in [
        b"td-mta-mime-evidence-v1\0".as_slice(),
        &length.to_be_bytes(),
        body_digest,
        &header_bytes.to_be_bytes(),
        &(table.len() as u64).to_be_bytes(),
        table,
        chunks,
    ] {
        hash.update(bytes).ok()?;
    }
    hash.finish().ok()
}

pub(crate) fn validate(
    length: u64,
    body_digest: &[u8; 32],
    prepared: &Prepared,
) -> Option<Evidence> {
    if length > crate::postgres_body::MAX_BODY_BYTES
        || prepared.header_bytes > HEADER_BYTES as u64
        || prepared.table.is_empty()
        || prepared.table.len() > PARTS * PART_BYTES
        || !prepared.table.len().is_multiple_of(PART_BYTES)
        || prepared.chunks.len() as u64 != length.div_ceil(CHUNK as u64) * 32
        || !Provider.equal_digest(
            &manifest_digest(
                length,
                body_digest,
                prepared.header_bytes,
                &prepared.table,
                &prepared.chunks,
            )?,
            &prepared.digest,
        )
    {
        return None;
    }
    let mut parts: Vec<Part> = Vec::new();
    parts
        .try_reserve_exact(prepared.table.len() / PART_BYTES)
        .ok()?;
    let mut ancestors = [None::<Part>; DEPTH];
    let mut previous = [None::<Part>; DEPTH];
    for bytes in prepared.table.as_chunks::<PART_BYTES>().0 {
        let part = decode(bytes)?;
        let depth = usize::from(part.depth);
        if part.ordinal as usize != parts.len() + 1
            || !(1..=DEPTH).contains(&depth)
            || part.entity_start > part.body_start
            || part.body_start > part.entity_end
            || part.entity_end > length
            || part.size > length
            || part.diagnostics
                & !(mime_structure::PROBLEM_FLAGS | mime_structure::DIGEST_CHILD_CONTEXT)
                != 0
            || (part.type_start != 0 || part.type_end != 0)
                && (part.type_start < part.entity_start
                    || part.type_start > part.type_end
                    || part.type_end > part.body_start)
        {
            return None;
        }
        if parts.is_empty() {
            if depth != 1 || part.parent != 0 || part.entity_start != 0 || part.entity_end != length
            {
                return None;
            }
        } else {
            let parent = ancestors.get(depth.checked_sub(2)?)?.as_ref()?;
            if parent.ordinal != part.parent
                || part.entity_start < parent.body_start
                || part.entity_end > parent.entity_end
                || previous.get(depth - 1)?.is_some_and(|previous| {
                    previous.parent == part.parent && previous.entity_end > part.entity_start
                })
            {
                return None;
            }
        }
        *ancestors.get_mut(depth - 1)? = Some(part);
        for deeper in ancestors.get_mut(depth..)? {
            *deeper = None;
        }
        *previous.get_mut(depth - 1)? = Some(part);
        parts.push(part);
    }
    Some(Evidence {
        parts,
        header_bytes: prepared.header_bytes,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn digest(bytes: &[u8]) -> [u8; 32] {
        let mut hash = BulkSha256::try_new().unwrap();
        hash.update(bytes).unwrap();
        hash.finish().unwrap()
    }

    #[test]
    fn retained_tree_preserves_nested_parts_and_decoded_sizes() {
        let bytes = b"Subject: example\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nhello\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--x--\r\n";
        let body = digest(bytes);
        let prepared = prepare(bytes, &body, Instant::now() + Duration::from_secs(30)).unwrap();
        let evidence = validate(bytes.len() as u64, &body, &prepared).unwrap();
        assert_eq!(evidence.parts.len(), 3);
        let attachment = evidence.parts.get(2).unwrap();
        assert_eq!(attachment.ordinal, 3);
        assert_eq!(attachment.parent, 1);
        assert_eq!(attachment.encoding, Encoding::Base64);
        assert_eq!(attachment.size, 3);
        assert_eq!(prepared.chunks, body);
        assert!(evidence.header_bytes > 0);
        assert!(evidence.header_bytes < bytes.len() as u64);
    }

    #[test]
    fn corrupt_evidence_or_a_different_blob_never_supplies_a_table() {
        let bytes = b"Content-Type: text/plain\r\n\r\nbody";
        let body = digest(bytes);
        let prepared = prepare(bytes, &body, Instant::now() + Duration::from_secs(30)).unwrap();
        assert!(validate(bytes.len() as u64, &digest(b"different"), &prepared).is_none());
        assert!(validate(bytes.len() as u64 + 1, &body, &prepared).is_none());
        let mut damaged = prepared.clone();
        *damaged.table.first_mut().unwrap() ^= 1;
        assert!(validate(bytes.len() as u64, &body, &damaged).is_none());
        let mut damaged = prepared.clone();
        *damaged.chunks.first_mut().unwrap() ^= 1;
        assert!(validate(bytes.len() as u64, &body, &damaged).is_none());
        let mut damaged = prepared;
        damaged.table.pop();
        assert!(validate(bytes.len() as u64, &body, &damaged).is_none());
    }

    #[test]
    fn empty_body_and_expired_preparation_keep_their_distinct_meanings() {
        let body = digest(b"");
        assert!(prepare(b"", &body, Instant::now()).is_none());
        let prepared = prepare(b"", &body, Instant::now() + Duration::from_secs(30)).unwrap();
        assert!(prepared.chunks.is_empty());
        assert_eq!(validate(0, &body, &prepared).unwrap().parts.len(), 1);
    }

    #[test]
    fn optional_preparation_cannot_replenish_an_exhausted_original_meter() {
        let bytes = b"Content-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--x--\r\n";
        let clock = RuntimeClock::new();
        let deadline = Instant::now() + Duration::from_secs(3600);
        for empty in 0..3 {
            let mut credit = Charge {
                io_bytes: 1_000_000,
                records: 1_000_000,
                output_bytes: 1_000_000,
                ..Charge::default()
            };
            match empty {
                0 => credit.io_bytes = 0,
                1 => credit.records = 0,
                _ => credit.output_bytes = 0,
            }
            let mut original = Meter::new(clock.deadline_before(deadline).unwrap(), credit);
            assert!(
                prepare_metered(bytes, &digest(bytes), deadline, &mut original, &clock).is_none()
            );
            assert_eq!(original.remaining(), credit);
            assert!(original.stopped().is_none());
        }
        let mut original = meter(&clock, deadline).unwrap();
        let before = original.remaining();
        assert!(prepare_metered(bytes, &digest(bytes), deadline, &mut original, &clock).is_some());
        let after = original.remaining();
        assert!(
            after.io_bytes < before.io_bytes
                && after.records < before.records
                && after.output_bytes < before.output_bytes
        );
    }

    #[test]
    fn large_encoded_attachment_has_identical_publication_and_backfill_evidence() {
        let mut bytes =
            b"Content-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\n"
                .to_vec();
        let line = [b"QUFB".repeat(19), b"\r\n".to_vec()].concat();
        for _ in 0..(2 * 1024 * 1024usize).div_ceil(57) {
            bytes.extend_from_slice(&line);
        }
        let body = digest(&bytes);
        let clock = RuntimeClock::new();
        let deadline = Instant::now() + Duration::from_secs(3600);
        let mut original = meter(&clock, deadline).unwrap();
        let publication = prepare_metered(&bytes, &body, deadline, &mut original, &clock).unwrap();
        let evidence = validate(bytes.len() as u64, &body, &publication).unwrap();
        assert!(evidence.parts.first().unwrap().size >= 2 * 1024 * 1024);
        struct FixedClock;
        impl Clock for FixedClock {
            fn sample(&self) -> Result<crate::ports::Time, crate::ports::Error> {
                Ok(crate::ports::Time {
                    monotonic: crate::ports::Tick(1),
                    utc_ms: 0,
                })
            }
        }
        crate::store_fs::with_pinned_fixture(&bytes, &FixedClock, |mut pin| {
            let rebuilt = prepare_verified(&mut pin, &body, deadline)
                .unwrap()
                .unwrap();
            assert_eq!(rebuilt.table, publication.table);
            assert_eq!(rebuilt.chunks, publication.chunks);
            assert_eq!(rebuilt.digest, publication.digest);
        });
    }
}
