//! Immutable configuration inputs, not validated or activated service policy.
use crate::{
    ids::StoreEpoch,
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

const CHUNK: usize = 65536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i16)]
pub enum Kind {
    RelayPassword = 1,
    CertificateChain = 2,
    PrivateKey = 3,
    TrustBundle = 4,
    TextSignature = 5,
    HtmlSignature = 6,
}
impl Kind {
    pub const fn max_bytes(self) -> usize {
        match self {
            Self::RelayPassword => 1026,
            Self::CertificateChain => 65536,
            Self::PrivateKey | Self::TextSignature | Self::HtmlSignature => 16384,
            Self::TrustBundle => 131072,
        }
    }
    fn accepts(self, length: usize) -> bool {
        length <= self.max_bytes()
            && (length > 0 || matches!(self, Self::TextSignature | Self::HtmlSignature))
    }
}

pub use crate::ids::MaterialVersion as Version;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Schema(postgres_schema::Error),
    Connection(postgres_probe::Error),
    Epoch,
    Invalid,
    Missing,
    Kind,
    Integrity,
    Capacity,
    Crypto,
    Database,
    /// The version may be committed. Resolve it after the original operation
    /// settles; never replay blindly.
    Indeterminate,
}

/// Secret bytes deliberately have no Debug or automatic text conversion.
pub struct Material {
    kind: Kind,
    bytes: Vec<u8>,
}
impl Material {
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Append a caller-chosen fresh version. An uncertain result can be resolved
/// by reading this same version; never allocate another version and replay.
/// Bytes are only size-checked here. Content/provider validation and complete
/// generation publication are separate required boundaries.
pub fn insert(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    version: Version,
    kind: Kind,
    bytes: &[u8],
) -> Result<(), Error> {
    if !kind.accepts(bytes.len()) {
        return Err(Error::Invalid);
    }
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| Error::Capacity)?;
    owned.extend_from_slice(bytes);
    let checksum = hash(&owned)?;
    let length = i32::try_from(owned.len()).map_err(|_| Error::Invalid)?;
    let count = i16::try_from(owned.len().div_ceil(CHUNK)).map_err(|_| Error::Invalid)?;
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_material_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client)
                .await
                .map_err(Error::Schema)?;
            if receipt.store_epoch != epoch {
                return Err(Error::Epoch);
            }
            client
                .batch_execute("BEGIN; SET LOCAL synchronous_commit = on")
                .await
                .map_err(|_| Error::Database)?;
            let rows = client.execute("INSERT INTO td_mta.config_materials (id, kind, length, checksum, chunk_count) VALUES ($1, $2, $3, $4, $5)",
                &[&version.as_bytes().as_slice(), &(kind as i16), &length, &checksum.as_slice(), &count])
                .await.map_err(|_| Error::Database)?;
            if rows != 1 {
                return Err(Error::Integrity);
            }
            for (ordinal, chunk) in owned.chunks(CHUNK).enumerate() {
                let ordinal = i16::try_from(ordinal).map_err(|_| Error::Invalid)?;
                let rows = client.execute("INSERT INTO td_mta.config_material_chunks (material_id, ordinal, bytes) VALUES ($1, $2, $3)",
                    &[&version.as_bytes().as_slice(), &ordinal, &chunk])
                    .await.map_err(|_| Error::Database)?;
                if rows != 1 {
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
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

/// Read one exact version and expected kind under a finite primary snapshot.
/// No raw bytes escape until length, ordinals and digest have all matched.
pub fn read(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    version: Version,
    kind: Kind,
) -> Result<Material, Error> {
    let result = postgres_probe::with_material_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if receipt.store_epoch != epoch { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .await.map_err(|_| Error::Database)?;
            let rows = client.query("SELECT kind, length, checksum, chunk_count FROM td_mta.config_materials WHERE id = $1 LIMIT 2",
                &[&version.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if rows.is_empty() { return Err(Error::Missing); }
            if rows.len() != 1 { return Err(Error::Integrity); }
            let row = rows.first().ok_or(Error::Integrity)?;
            let stored_kind: i16 = row.try_get(0).map_err(|_| Error::Integrity)?;
            if stored_kind != kind as i16 { return Err(Error::Kind); }
            let length: i32 = row.try_get(1).map_err(|_| Error::Integrity)?;
            let length = usize::try_from(length).map_err(|_| Error::Integrity)?;
            let digest: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
            let count: i16 = row.try_get(3).map_err(|_| Error::Integrity)?;
            if !kind.accepts(length) || digest.len() != 32
                || usize::try_from(count).ok() != Some(length.div_ceil(CHUNK)) {
                return Err(Error::Integrity);
            }
            let chunks = client.query("SELECT ordinal, bytes FROM td_mta.config_material_chunks WHERE material_id = $1 ORDER BY ordinal LIMIT 3",
                &[&version.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if chunks.len() != length.div_ceil(CHUNK) { return Err(Error::Integrity); }
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(length).map_err(|_| Error::Capacity)?;
            for (index, chunk) in chunks.iter().enumerate() {
                let ordinal: i16 = chunk.try_get(0).map_err(|_| Error::Integrity)?;
                let data: &[u8] = chunk.try_get(1).map_err(|_| Error::Integrity)?;
                if usize::try_from(ordinal).ok() != Some(index)
                    || data.len() != length.saturating_sub(bytes.len()).min(CHUNK) {
                    return Err(Error::Integrity);
                }
                bytes.extend_from_slice(data);
            }
            if bytes.len() != length || hash(&bytes)?.as_slice() != digest {
                return Err(Error::Integrity);
            }
            Ok(Material { kind, bytes })
        })
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

pub fn fresh_version() -> Result<Version, Error> {
    let mut bytes = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut rng| rng.fill(&mut bytes))
        .map_err(|_| Error::Crypto)?;
    Ok(Version::from_bytes(bytes))
}

fn hash(bytes: &[u8]) -> Result<[u8; 32], Error> {
    let mut digest = Provider.sha256().map_err(|_| Error::Crypto)?;
    digest.update(bytes).map_err(|_| Error::Crypto)?;
    digest.finish().map_err(|_| Error::Crypto)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_material_bounds_match_existing_input_policy() {
        for kind in [
            Kind::RelayPassword,
            Kind::CertificateChain,
            Kind::PrivateKey,
            Kind::TrustBundle,
            Kind::TextSignature,
            Kind::HtmlSignature,
        ] {
            assert!(kind.accepts(kind.max_bytes()));
            assert!(!kind.accepts(kind.max_bytes() + 1));
            assert_eq!(
                kind.accepts(0),
                matches!(kind, Kind::TextSignature | Kind::HtmlSignature)
            );
        }
        assert_eq!(
            Kind::RelayPassword.max_bytes(),
            crate::config::inputs::Target::RelayPassword.maximum_bytes()
        );
        assert_eq!(
            Kind::HtmlSignature.max_bytes(),
            crate::config::material::MAX_SIGNATURE_BYTES
        );
        assert_eq!(
            Kind::CertificateChain.max_bytes(),
            crate::config::inputs::MAX_CHAIN_BYTES
        );
        assert_eq!(
            Kind::PrivateKey.max_bytes(),
            crate::config::inputs::MAX_KEY_BYTES
        );
        assert_eq!(
            Kind::TrustBundle.max_bytes(),
            crate::config::inputs::MAX_CA_BYTES
        );
        assert_eq!(
            Kind::TextSignature.max_bytes(),
            crate::config::material::MAX_SIGNATURE_BYTES
        );
    }
}
