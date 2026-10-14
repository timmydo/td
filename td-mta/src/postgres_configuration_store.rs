//! Immutable owner-side source versions. No active pointer or serving authority.
use crate::{
    config::profile::Roles,
    ids::StoreEpoch,
    postgres_configuration::{self as configuration, Candidate, Profile},
    postgres_probe::{self, Request},
    postgres_schema,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_crypto::{Crypto, Digest, Entropy, Provider, SystemEntropy, TrustStore};

pub use crate::ids::ConfigurationVersion as Version;
const CHUNK: usize = 65536;
const MAX_WIRE: usize = configuration::MAX_SOURCE_BYTES + 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Validation(configuration::Error),
    Epoch,
    Missing,
    Conflict,
    Integrity,
    Capacity,
    Crypto,
    Database,
    /// This exact version may have committed; retain its ID and source digest.
    Indeterminate,
}

pub fn fresh_version() -> Result<Version, Error> {
    let mut bytes = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut rng| rng.fill(&mut bytes))
        .map_err(|_| Error::Crypto)?;
    Ok(Version::from_bytes(bytes))
}

/// Publish an already validated source set. Reusing an ID with the same digest
/// resolves its durable receipt; different sources conflict. This never selects
/// a version for runtime use. Keep the same ID after an uncertain commit.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    version: Version,
    candidate: &Candidate,
) -> Result<(), Error> {
    if deadline
        .checked_duration_since(Instant::now())
        .filter(|allowance| !allowance.is_zero() && *allowance <= postgres_probe::MAX_DURATION)
        .is_none()
    {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(MAX_WIRE)
        .map_err(|_| Error::Capacity)?;
    configuration::visit_bytes(candidate, |part| {
        if bytes
            .len()
            .checked_add(part.len())
            .filter(|n| *n <= MAX_WIRE)
            .is_none()
        {
            return Err(configuration::Error::Capacity);
        }
        bytes.extend_from_slice(part);
        Ok(())
    })
    .map_err(Error::Validation)?;
    let epoch = candidate.epoch();
    let digest = *candidate.digest();
    let length = i32::try_from(bytes.len()).map_err(|_| Error::Capacity)?;
    let count = i16::try_from(bytes.len().div_ceil(CHUNK)).map_err(|_| Error::Capacity)?;
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_configuration_client(
        request,
        trust,
        deadline,
        move |client| {
            Box::pin(async move {
                let receipt = postgres_schema::lock_and_inspect(client)
                    .await
                    .map_err(Error::Schema)?;
                if receipt.store_epoch != epoch {
                    return Err(Error::Epoch);
                }
                client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit = on; SET LOCAL idle_in_transaction_session_timeout = 10000")
                .await.map_err(|_| Error::Database)?;
                let inserted = client.execute("INSERT INTO td_mta.config_versions (id,length,checksum,chunk_count) VALUES ($1,$2,$3,$4) ON CONFLICT (id) DO NOTHING", &[&version.as_bytes().as_slice(), &length, &digest.as_slice(), &count])
                .await.map_err(|_| Error::Database)?;
                if inserted == 0 {
                    let row = client.query_one("SELECT length,checksum,chunk_count FROM td_mta.config_versions WHERE id=$1", &[&version.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
                    let stored_length: i32 = row.try_get(0).map_err(|_| Error::Integrity)?;
                    let stored_digest: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
                    let stored_count: i16 = row.try_get(2).map_err(|_| Error::Integrity)?;
                    if stored_length != length || stored_digest != digest || stored_count != count {
                        return Err(Error::Conflict);
                    }
                    return Ok(());
                }
                if inserted != 1 {
                    return Err(Error::Integrity);
                }
                for (ordinal, chunk) in bytes.chunks(CHUNK).enumerate() {
                    let ordinal = i16::try_from(ordinal).map_err(|_| Error::Capacity)?;
                    let changed = client.execute("INSERT INTO td_mta.config_version_chunks (version_id,ordinal,bytes) VALUES ($1,$2,$3)", &[&version.as_bytes().as_slice(), &ordinal, &chunk]).await.map_err(|_| Error::Database)?;
                    if changed != 1 {
                        return Err(Error::Integrity);
                    }
                }
                marker.store(true, Ordering::Relaxed);
                client
                    .batch_execute("COMMIT")
                    .await
                    .map_err(|_| Error::Indeterminate)?;
                Ok(())
            })
        },
    );
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

/// Read and verify one primary snapshot, then revalidate every profile/material
/// under the original deadline. Raw or stale configuration never escapes.
pub fn read(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    version: Version,
) -> Result<Candidate, Error> {
    let bytes = postgres_probe::with_configuration_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if receipt.store_epoch != epoch { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout = 10000")
                .await.map_err(|_| Error::Database)?;
            let rows = client.query("SELECT length,checksum,chunk_count FROM td_mta.config_versions WHERE id=$1 LIMIT 2", &[&version.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if rows.is_empty() { return Err(Error::Missing); }
            if rows.len() != 1 { return Err(Error::Integrity); }
            let row = rows.first().ok_or(Error::Integrity)?;
            let length: i32 = row.try_get(0).map_err(|_| Error::Integrity)?;
            let length = usize::try_from(length).map_err(|_| Error::Integrity)?;
            let digest: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
            let count: i16 = row.try_get(2).map_err(|_| Error::Integrity)?;
            if length == 0 || length > MAX_WIRE || digest.len() != 32 || usize::try_from(count).ok() != Some(length.div_ceil(CHUNK)) {
                return Err(Error::Integrity);
            }
            let chunks = client.query("SELECT ordinal,bytes FROM td_mta.config_version_chunks WHERE version_id=$1 ORDER BY ordinal LIMIT 34", &[&version.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if chunks.len() != length.div_ceil(CHUNK) { return Err(Error::Integrity); }
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(length).map_err(|_| Error::Capacity)?;
            for (index, row) in chunks.iter().enumerate() {
                let ordinal: i16 = row.try_get(0).map_err(|_| Error::Integrity)?;
                let part: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
                if usize::try_from(ordinal).ok() != Some(index) || part.len() != length.saturating_sub(bytes.len()).min(CHUNK) {
                    return Err(Error::Integrity);
                }
                bytes.extend_from_slice(part);
            }
            let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
            hash.update(&bytes).map_err(|_| Error::Crypto)?;
            if hash.finish().map_err(|_| Error::Crypto)?.as_slice() != digest { return Err(Error::Integrity); }
            Ok(bytes)
        })
    }).map_err(Error::Connection)??;
    let (shared, profiles) = decode(&bytes, epoch)?;
    configuration::prepare(request, trust, deadline, epoch, shared, &profiles)
        .map_err(Error::Validation)
}

fn take<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8], Error> {
    let part = bytes.get(..count).ok_or(Error::Integrity)?;
    *bytes = bytes.get(count..).ok_or(Error::Integrity)?;
    Ok(part)
}
fn number(bytes: &[u8]) -> Result<usize, Error> {
    let value = u64::from_be_bytes(bytes.try_into().map_err(|_| Error::Integrity)?);
    usize::try_from(value).map_err(|_| Error::Integrity)
}
fn field<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], Error> {
    let count = number(take(bytes, 8)?)?;
    take(bytes, count)
}
fn decode(mut bytes: &[u8], epoch: StoreEpoch) -> Result<(&[u8], Vec<Profile<'_>>), Error> {
    if bytes.len() > MAX_WIRE
        || take(&mut bytes, configuration::DIGEST_DOMAIN.len())? != configuration::DIGEST_DOMAIN
    {
        return Err(Error::Integrity);
    }
    if take(&mut bytes, 16)? != epoch.as_bytes() {
        return Err(Error::Epoch);
    }
    let shared = field(&mut bytes)?;
    let count = number(field(&mut bytes)?)?;
    if count == 0 || count > configuration::MAX_PROFILES {
        return Err(Error::Integrity);
    }
    let mut profiles: Vec<Profile<'_>> = Vec::new();
    profiles
        .try_reserve_exact(count)
        .map_err(|_| Error::Capacity)?;
    let mut size = shared.len();
    for _ in 0..count {
        let name = std::str::from_utf8(field(&mut bytes)?).map_err(|_| Error::Integrity)?;
        crate::config::values::profile_name(name).map_err(|_| Error::Integrity)?;
        if profiles.last().is_some_and(|last| last.name >= name) {
            return Err(Error::Integrity);
        }
        let role = field(&mut bytes)?;
        let [bits] = role else {
            return Err(Error::Integrity);
        };
        let roles = Roles::from_bits(*bits).ok_or(Error::Integrity)?;
        let source = field(&mut bytes)?;
        size = size
            .checked_add(name.len() + 1)
            .and_then(|n| n.checked_add(source.len()))
            .ok_or(Error::Integrity)?;
        if size > configuration::MAX_SOURCE_BYTES {
            return Err(Error::Integrity);
        }
        profiles.push(Profile {
            name,
            roles,
            source,
        });
    }
    if !bytes.is_empty() {
        return Err(Error::Integrity);
    }
    Ok((shared, profiles))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn encoded(names: &[&str]) -> Vec<u8> {
        let mut bytes = configuration::DIGEST_DOMAIN.to_vec();
        bytes.extend_from_slice(&[1; 16]);
        let mut field = |part: &[u8]| {
            bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
            bytes.extend_from_slice(part);
        };
        field(b"version = 1\n");
        field(&(names.len() as u64).to_be_bytes());
        for name in names {
            field(name.as_bytes());
            field(&[2]);
            field(b"[server]\n");
        }
        bytes
    }

    #[test]
    fn wire_decoder_refuses_truncation_noncanonical_inventory_and_trailing_bytes() {
        let epoch = StoreEpoch::from_bytes([1; 16]);
        let good = encoded(&["cloud", "desktop"]);
        let (shared, profiles) = decode(&good, epoch).unwrap();
        assert_eq!(shared, b"version = 1\n");
        assert_eq!(profiles.len(), 2);
        for length in 0..good.len() {
            assert!(decode(&good[..length], epoch).is_err());
        }
        for names in [
            &[][..],
            &["desktop", "cloud"][..],
            &["cloud", "cloud"][..],
            &["Cloud"][..],
        ] {
            assert!(decode(&encoded(names), epoch).is_err());
        }
        assert!(decode(&encoded(&["cloud"; 17]), epoch).is_err());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode(&trailing, epoch).is_err());
        assert_eq!(
            decode(&good, StoreEpoch::from_bytes([2; 16])).err(),
            Some(Error::Epoch)
        );
        let mut corrupt = good;
        corrupt[configuration::DIGEST_DOMAIN.len() + 16..configuration::DIGEST_DOMAIN.len() + 24]
            .fill(255);
        assert!(decode(&corrupt, epoch).is_err());
    }
}
