//! Complete search interpretation, rebuilt only from a verified immutable body.
use crate::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    jmap_request::dispatch::{collect_verified_search, SearchFields},
    limits::Limits,
    ports::Error,
    store_fs::PinnedBlob,
};
use std::time::Instant;
use td_crypto::{BulkSha256, Digest};
use tokio_postgres::{types::Type, Client};

pub(crate) const VERSION: i32 = SearchFields::VERSION as i32;
pub(crate) const MAX_BYTES: usize = SearchFields::MAX_ENCODED;
pub(crate) struct Prepared {
    pub(crate) fields: Vec<u8>,
    pub(crate) candidate_text: String,
    pub(crate) digest: [u8; 32],
}

pub(crate) async fn store(
    client: &Client,
    metadata: crate::postgres_body::BlobMetadata,
    value: Option<Prepared>,
) -> Result<bool, tokio_postgres::Error> {
    let account = metadata.identity.account;
    let blob = metadata.blob;
    if let Some(value) = value {
        client.execute_typed("INSERT INTO td_mta.blob_search(account,blob_id,version,body_length,body_digest,fields,fields_digest,candidate_text) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(account,blob_id) DO UPDATE SET version=EXCLUDED.version,body_length=EXCLUDED.body_length,body_digest=EXCLUDED.body_digest,fields=EXCLUDED.fields,fields_digest=EXCLUDED.fields_digest,candidate_text=EXCLUDED.candidate_text", &[(&account.as_bytes().as_slice(), Type::BYTEA), (&blob.as_bytes().as_slice(), Type::BYTEA), (&VERSION, Type::INT4), (&(metadata.body.length as i64), Type::INT8), (&metadata.body.digest.as_slice(), Type::BYTEA), (&value.fields, Type::BYTEA), (&value.digest.as_slice(), Type::BYTEA), (&value.candidate_text, Type::TEXT)]).await?;
        Ok(true)
    } else {
        // A refused rebuild must not retain earlier, incompatible evidence.
        client
            .execute_typed(
                "DELETE FROM td_mta.blob_search WHERE account=$1 AND blob_id=$2",
                &[
                    (&account.as_bytes().as_slice(), Type::BYTEA),
                    (&blob.as_bytes().as_slice(), Type::BYTEA),
                ],
            )
            .await?;
        Ok(false)
    }
}

pub(crate) fn compatible(limits: &Limits) -> bool {
    limits.header_bytes == 256 * 1024 && limits.mime_parts == 1024 && limits.mime_depth == 32
}

pub(crate) fn digest(
    body_digest: &[u8; 32],
    body_length: u64,
    fields: &[u8],
    deadline: Instant,
) -> Result<[u8; 32], Error> {
    if fields.len() > MAX_BYTES {
        return Err(Error::Invalid);
    }
    check_deadline(deadline)?;
    let mut hash = BulkSha256::try_new()?;
    hash.update(b"td-mta-search-fields-v1\0")?;
    hash.update(body_digest)?;
    hash.update(&body_length.to_be_bytes())?;
    hash.update(&(fields.len() as u64).to_be_bytes())?;
    for chunk in fields.chunks(65536) {
        check_deadline(deadline)?;
        hash.update(chunk)?;
    }
    let result = hash.finish()?;
    check_deadline(deadline)?;
    Ok(result)
}

fn check_deadline(deadline: Instant) -> Result<(), Error> {
    if Instant::now() >= deadline {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}

fn operation(error: crate::jmap_request::dispatch::Error) -> Error {
    use crate::jmap_request::dispatch::Error as Search;
    match error {
        Search::Storage(error) => error,
        Search::ExecutionDeadline => Error::Deadline,
        Search::StorageCapacity | Search::ResponseCapacity => Error::Capacity,
        _ => Error::Invalid,
    }
}

/// Only interpretation refusals and permanent representation bounds are skips.
/// Operational failures abort the administrative transaction.
pub(crate) fn prepare(
    pin: &mut PinnedBlob<'_, '_>,
    deadline: Instant,
) -> Result<Option<Prepared>, Error> {
    use crate::ports::BlobReader;
    check_deadline(deadline)?;
    let clock = RuntimeClock::new();
    let mut work = Meter::new(
        clock.deadline_before(deadline)?,
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
    let collected = collect_verified_search(pin, &limits, &clock, &mut work);
    pin.check_deadline()?;
    check_deadline(deadline)?;
    let fields = match collected {
        Ok(Some(fields)) => fields.encode().map_err(operation)?,
        Ok(None) | Err(crate::jmap_request::dispatch::Error::MessageInterpretation) => {
            return Ok(None);
        }
        Err(error) => return Err(operation(error)),
    };
    pin.check_deadline()?;
    let digest = digest(pin.digest(), pin.len(), &fields, deadline)?;
    let candidate_text = candidate_text(&fields, deadline)?;
    Ok(Some(Prepared {
        fields,
        candidate_text,
        digest,
    }))
}

// Keep field boundaries and replace PostgreSQL text's forbidden NUL. Operands
// containing NUL are never used as index filters; the matcher sees original fields.
fn candidate_text(encoded: &[u8], deadline: Instant) -> Result<String, Error> {
    if encoded.len() > MAX_BYTES {
        return Err(Error::Invalid);
    }
    check_deadline(deadline)?;
    let mut text = Vec::new();
    text.try_reserve_exact(encoded.len())
        .map_err(|_| Error::Capacity)?;
    let mut rest = encoded;
    let mut count = 0usize;
    while !rest.is_empty() {
        count = count.checked_add(1).ok_or(Error::Invalid)?;
        check_deadline(deadline)?;
        if count > SearchFields::MAX_FIELDS {
            return Err(Error::Invalid);
        }
        let size = u32::from_be_bytes(
            rest.get(..4)
                .ok_or(Error::Invalid)?
                .try_into()
                .map_err(|_| Error::Invalid)?,
        ) as usize;
        rest = rest.get(4..).ok_or(Error::Invalid)?;
        let field = rest.get(..size).ok_or(Error::Invalid)?;
        std::str::from_utf8(field).map_err(|_| Error::Invalid)?;
        rest = rest.get(size..).ok_or(Error::Invalid)?;
        for chunk in field.chunks(65536) {
            check_deadline(deadline)?;
            text.extend(
                chunk
                    .iter()
                    .map(|byte| if *byte == 0 { b'\n' } else { *byte }),
            );
        }
        text.push(b'\n');
    }
    check_deadline(deadline)?;
    if text.len() > SearchFields::MAX_BYTES + SearchFields::MAX_FIELDS {
        return Err(Error::Invalid);
    }
    String::from_utf8(text).map_err(|_| Error::Invalid)
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
            Ok("αβγ straße\n100%_\\\nnul\nvalue\n".into())
        );
        assert_eq!(candidate_text(&[], deadline), Ok(String::new()));
    }
    #[test]
    fn candidate_text_refuses_bad_framing_utf8_and_expiry() {
        let deadline = Instant::now() + std::time::Duration::from_secs(3600);
        for bytes in [
            &[0, 0, 0][..],
            &[0, 0, 0, 2, b'a'][..],
            &[0, 0, 0, 1, 255][..],
        ] {
            assert_eq!(candidate_text(bytes, deadline), Err(Error::Invalid));
        }
        assert_eq!(
            candidate_text(&encoded(&["x"]), Instant::now()),
            Err(Error::Deadline)
        );
    }

    #[test]
    fn digest_preserves_expiry_as_an_operational_error() {
        assert_eq!(
            digest(&[0; 32], 0, &[], Instant::now()),
            Err(Error::Deadline)
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(3600);
        assert!(digest(&[0; 32], 0, &[], deadline).is_ok());
    }

    #[test]
    fn preparation_skips_interpretation_but_refuses_expired_work() {
        use crate::ports::{Clock, Tick, Time};
        struct Fixed;
        impl Clock for Fixed {
            fn sample(&self) -> Result<Time, Error> {
                Ok(Time {
                    utc_ms: 0,
                    monotonic: Tick(1),
                })
            }
        }
        crate::store_fs::with_pinned_fixture(
            b"Content-Type: message/global\r\n\r\nbody",
            &Fixed,
            |mut pin| {
                assert!(matches!(
                    prepare(&mut pin, Instant::now()),
                    Err(Error::Deadline)
                ));
                assert!(matches!(
                    prepare(
                        &mut pin,
                        Instant::now() + std::time::Duration::from_secs(3600)
                    ),
                    Ok(None)
                ));
            },
        );
    }
}
