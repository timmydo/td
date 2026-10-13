//! Cold preparation of the default Identity/get objects td-mail reads.
use crate::{
    bounded::TextBuffer,
    config::{identities::AddressIter, identity, materialize::ResolvedText, preimage},
    ids::{AccountId, IdentityId},
    ports::Error,
    sync::IdentityState,
    wire,
};
use std::ops::Range;
use td_crypto::{Crypto, Digest};

pub const MAX_BYTES: usize = identity::MAX_PREIMAGE_BYTES * 6 + identity::MAX_IDENTITIES * 512;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparationError {
    Representation(identity::Error),
    Adapter(Error),
}
impl From<Error> for PreparationError {
    fn from(error: Error) -> Self {
        Self::Adapter(error)
    }
}
impl From<td_crypto::Error> for PreparationError {
    fn from(error: td_crypto::Error) -> Self {
        Self::Adapter(error.into())
    }
}
impl std::fmt::Display for PreparationError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Representation(error) => error.fmt(out),
            Self::Adapter(error) => error.fmt(out),
        }
    }
}
impl std::error::Error for PreparationError {}
struct Entry {
    id: IdentityId,
    bytes: Range<usize>,
}
/// Immutable visible data only. Preparation requires trusted resolved config;
/// this owner confers no authentication or submission authority.
pub struct Catalog {
    account: AccountId,
    digest: [u8; 32],
    bytes: Vec<u8>,
    entries: Vec<Entry>,
}
impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IdentityCatalog(<redacted>)")
    }
}
impl Catalog {
    /// Run on the cold configuration worker, before exposing a generation.
    /// The canonical preimage validates aggregate visible-data limits first.
    pub fn prepare(
        configuration: &ResolvedText,
        crypto: &impl Crypto,
    ) -> Result<Self, PreparationError> {
        let mut hash = crypto.sha256()?;
        preimage::write(configuration, |bytes| hash.update(bytes)).map_err(
            |error| match error {
                preimage::Error::Sink(error) => PreparationError::from(error),
                preimage::Error::Invariant => Error::Corrupt.into(),
                preimage::Error::Invalid(error) => PreparationError::Representation(error),
            },
        )?;
        let digest = hash.finish()?;
        let records = configuration
            .candidate()
            .identities()
            .map_err(|_| Error::Corrupt)?;
        let account = records.account().map_err(|_| Error::Corrupt)?.id;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(MAX_BYTES)
            .map_err(|_| Error::Capacity)?;
        bytes.resize(MAX_BYTES, 0);
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(records.len())
            .map_err(|_| Error::Capacity)?;
        let length = {
            let mut out = TextBuffer::new(&mut bytes);
            for index in 0..records.len() {
                let record = records
                    .identity(index)
                    .map_err(|_| Error::Corrupt)?
                    .ok_or(Error::Corrupt)?;
                let signatures = configuration
                    .signatures(index)
                    .map_err(|_| Error::Corrupt)?
                    .ok_or(Error::Corrupt)?;
                let start = out.len();
                let mut wire_bytes = [0; wire::IdKind::WIRE_BYTES];
                let id = wire::encode_id(record.id, &mut wire_bytes).map_err(|_| Error::Corrupt)?;
                append(&mut out, "{\"id\":")?;
                quoted(&mut out, id)?;
                for (name, value) in [("name", record.name), ("email", record.email)] {
                    field(&mut out, name)?;
                    quoted(&mut out, value)?;
                }
                field(&mut out, "replyTo")?;
                addresses(&mut out, record.reply_to)?;
                field(&mut out, "bcc")?;
                addresses(&mut out, record.bcc)?;
                for (name, value) in [
                    ("textSignature", signatures.text),
                    ("htmlSignature", signatures.html),
                ] {
                    field(&mut out, name)?;
                    quoted(&mut out, value)?;
                }
                append(&mut out, ",\"mayDelete\":false}")?;
                entries.push(Entry {
                    id: record.id,
                    bytes: start..out.len(),
                });
            }
            out.len()
        };
        let mut retained = Vec::new();
        retained
            .try_reserve_exact(length)
            .map_err(|_| Error::Capacity)?;
        retained.extend_from_slice(bytes.get(..length).ok_or(Error::Corrupt)?);
        drop(bytes);
        let bytes = retained;
        Ok(Self {
            account,
            digest,
            bytes,
            entries,
        })
    }
    pub const fn account(&self) -> AccountId {
        self.account
    }
    /// The serving account snapshot supplies its current store epoch.
    pub fn state(&self, snapshot: crate::ports::ViewIdentity) -> Result<IdentityState, Error> {
        if snapshot.account != self.account {
            return Err(Error::Invalid);
        }
        Ok(IdentityState {
            account: self.account,
            epoch: snapshot.epoch,
            digest: self.digest,
        })
    }
    pub fn ids(&self) -> impl ExactSizeIterator<Item = IdentityId> + '_ {
        self.entries.iter().map(|entry| entry.id)
    }
    pub fn object(&self, id: IdentityId) -> Result<Option<&[u8]>, Error> {
        let Ok(index) = self.entries.binary_search_by_key(&id, |entry| entry.id) else {
            return Ok(None);
        };
        let entry = self.entries.get(index).ok_or(Error::Corrupt)?;
        self.bytes
            .get(entry.bytes.clone())
            .map(Some)
            .ok_or(Error::Corrupt)
    }
}
fn append(out: &mut TextBuffer<'_>, value: &str) -> Result<(), Error> {
    out.append(value).map_err(|_| Error::Capacity)
}
fn quoted(out: &mut TextBuffer<'_>, value: &str) -> Result<(), Error> {
    if value.chars().any(crate::unicode::is_noncharacter) {
        return Err(Error::Invalid);
    }
    crate::jmap_session::quoted(out, value)
}
fn field(out: &mut TextBuffer<'_>, name: &str) -> Result<(), Error> {
    append(out, ",")?;
    quoted(out, name)?;
    append(out, ":")
}
fn addresses(
    out: &mut TextBuffer<'_>,
    addresses: Option<AddressIter<'_, '_>>,
) -> Result<(), Error> {
    let Some(addresses) = addresses else {
        return append(out, "null");
    };
    append(out, "[")?;
    for (index, address) in addresses.enumerate() {
        let address = address.map_err(|_| Error::Corrupt)?;
        if index != 0 {
            append(out, ",")?;
        }
        append(out, "{\"name\":")?;
        if let Some(name) = address.name {
            quoted(out, name)?;
        } else {
            append(out, "null")?;
        }
        field(out, "email")?;
        quoted(out, address.email)?;
        append(out, "}")?;
    }
    append(out, "]")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{format::Sequence, ids::StoreEpoch, ports::ViewIdentity};
    fn snapshot(account: AccountId, epoch: StoreEpoch) -> ViewIdentity {
        ViewIdentity {
            account,
            epoch,
            committed_sequence: Sequence::from_u64(0),
            history_floor: Sequence::from_u64(0),
        }
    }
    fn state(catalog: &Catalog, epoch: StoreEpoch) -> IdentityState {
        catalog.state(snapshot(catalog.account(), epoch)).unwrap()
    }
    use crate::config::preimage::tests::{lock, resolved, BASE};
    #[test]
    fn default_objects_are_read_only_and_exclude_private_configuration() {
        let _lock = lock();
        let config = resolved(BASE);
        let catalog = Catalog::prepare(&config, &td_crypto::Provider).unwrap();
        let id = IdentityId::from_bytes([0xbb; 16]);
        assert_eq!(catalog.ids().collect::<Vec<_>>(), [id]);
        assert_eq!(catalog.account(), AccountId::from_bytes([0xaa; 16]));
        let bytes = catalog.object(id).unwrap().unwrap();
        let json = td_json::parse(std::str::from_utf8(bytes).unwrap()).unwrap();
        assert_eq!(
            json,
            td_json::json!({"id":"ibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "name":"", "email":"main@example.test", "replyTo":null,"bcc":null,"textSignature":"","htmlSignature":"","mayDelete":false})
        );
        assert!(catalog
            .object(IdentityId::from_bytes([0xcc; 16]))
            .unwrap()
            .is_none());
        assert_eq!(format!("{catalog:?}"), "IdentityCatalog(<redacted>)");
        assert!(!String::from_utf8_lossy(&catalog.bytes).contains("private"));
        assert!(catalog.bytes.capacity() < 1024);
        assert_eq!(
            catalog.state(snapshot(
                AccountId::from_bytes([0xcc; 16]),
                StoreEpoch::from_bytes([7; 16])
            )),
            Err(Error::Invalid)
        );
    }
    #[test]
    fn visible_name_and_epoch_bind_state_but_paths_do_not() {
        let _lock = lock();
        let source = BASE.replace("[resolver \"primary\"]", "name = \"Main é\"\nreply_to = true\nbcc = true\ntext_signature_file = \"/text\"\nhtml_signature_file = \"/html\"\n[resolver \"primary\"]")
            + "[identity_address \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"]\nkind = \"reply_to\"\nemail = \"reply@example.test\"\n";
        let epoch = StoreEpoch::from_bytes([7; 16]);
        let first = Catalog::prepare(&resolved(&source), &td_crypto::Provider).unwrap();
        let object = first
            .object(IdentityId::from_bytes([0xbb; 16]))
            .unwrap()
            .unwrap();
        let json = td_json::parse(std::str::from_utf8(object).unwrap()).unwrap();
        assert_eq!(json["name"].as_str(), Some("Main é"));
        assert_eq!(
            json["replyTo"][0]["email"].as_str(),
            Some("reply@example.test")
        );
        assert!(json["replyTo"][0]["name"].is_null());
        assert_eq!(json["bcc"].as_array().unwrap().len(), 0);
        assert_eq!(json["textSignature"].as_str(), Some("T\n"));
        assert_eq!(json["htmlSignature"].as_str(), Some("<b>H</b>"));
        assert!(!std::str::from_utf8(object).unwrap().contains("/text"));
        assert!(!std::str::from_utf8(object).unwrap().contains("/html"));
        let changed_default = Catalog::prepare(
            &resolved(&source.replace("bcc = true", "bcc = false")),
            &td_crypto::Provider,
        )
        .unwrap();
        assert_ne!(state(&first, epoch), state(&changed_default, epoch));
        let moved = source
            .replace("/text", "/other-text")
            .replace("/private-password", "/other-secret");
        let second = Catalog::prepare(&resolved(&moved), &td_crypto::Provider).unwrap();
        assert_eq!(state(&first, epoch), state(&second, epoch));
        let changed = Catalog::prepare(
            &resolved(&source.replace("Main é", "Other")),
            &td_crypto::Provider,
        )
        .unwrap();
        assert_ne!(state(&first, epoch), state(&changed, epoch));
        assert_ne!(
            state(&first, epoch),
            state(&first, StoreEpoch::from_bytes([8; 16]))
        );
    }
    #[test]
    fn configured_order_is_canonical_and_digest_matches_literal_oracle() {
        let _lock = lock();
        let source = BASE
            .replace(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "11111111111111111111111111111111",
            )
            .replace("main@example.test", "me@example.org")
            .replace("[domain \"example.test\"]", "[domain \"example.org\"]")
            .replace(
                "[resolver \"primary\"]",
                "name = \"Me\"\nbcc = true\n[resolver \"primary\"]",
            );
        let one = Catalog::prepare(&resolved(&source), &td_crypto::Provider).unwrap();
        let expected = [
            0x5b, 0x5b, 0xb9, 0xf2, 0x7c, 0xd7, 0xa8, 0x2b, 0xbc, 0x8e, 0x09, 0xdb, 0x63, 0x82,
            0x5a, 0xd7, 0x30, 0x61, 0x25, 0x15, 0xf8, 0x7d, 0x87, 0x38, 0x45, 0xe0, 0x98, 0x08,
            0x64, 0x07, 0x47, 0xce,
        ];
        assert_eq!(
            state(&one, StoreEpoch::from_bytes([7; 16])).digest,
            expected
        );
        let many = format!("{BASE}[identity \"11111111111111111111111111111111\"]\naccount = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nemail = \"other@example.test\"\n");
        let catalog = Catalog::prepare(&resolved(&many), &td_crypto::Provider).unwrap();
        let ids = [
            IdentityId::from_bytes([0x11; 16]),
            IdentityId::from_bytes([0xbb; 16]),
        ];
        assert_eq!(catalog.ids().collect::<Vec<_>>(), ids);
        for (id, email) in ids
            .into_iter()
            .zip(["other@example.test", "main@example.test"])
        {
            let object = catalog.object(id).unwrap().unwrap();
            let json = td_json::parse(std::str::from_utf8(object).unwrap()).unwrap();
            assert_eq!(json["email"].as_str(), Some(email));
            let mut wire_bytes = [0; wire::IdKind::WIRE_BYTES];
            assert_eq!(
                json["id"].as_str(),
                Some(wire::encode_id(id, &mut wire_bytes).unwrap())
            );
        }
    }
    #[test]
    fn full_identity_count_near_preimage_limit_and_refusal_keep_exact_bounds() {
        let _lock = lock();
        fn source(name: &str) -> String {
            let mut source = BASE
                .replace(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "00000000000000000000000000000001",
                )
                .replace(
                    "[resolver \"primary\"]",
                    &format!("name = \"{name}\"\n[resolver \"primary\"]"),
                );
            for n in 2..=64 {
                source.push_str(&format!("[identity \"{n:032x}\"]\naccount = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nname = \"{name}\"\nemail = \"main@example.test\"\n"));
            }
            source
        }
        let catalog =
            Catalog::prepare(&resolved(&source(&"x".repeat(2980))), &td_crypto::Provider).unwrap();
        assert_eq!(catalog.ids().len(), 64);
        assert!(catalog.bytes.len() > 192 * 1024);
        assert!(catalog.bytes.len() < MAX_BYTES);
        for id in catalog.ids() {
            let object = catalog.object(id).unwrap().unwrap();
            let value = td_json::parse(std::str::from_utf8(object).unwrap()).unwrap();
            assert_eq!(value["name"].as_str().unwrap().len(), 2980);
            let mut wire_bytes = [0; wire::IdKind::WIRE_BYTES];
            assert_eq!(
                value["id"].as_str(),
                Some(wire::encode_id(id, &mut wire_bytes).unwrap())
            );
        }
        let error = Catalog::prepare(&resolved(&source(&"x".repeat(3020))), &td_crypto::Provider)
            .unwrap_err();
        assert_eq!(
            error,
            PreparationError::Representation(identity::Error::PreimageLimit)
        );
        assert_eq!(error.to_string(), "identity_preimage_size_limit");
    }
    #[test]
    fn noncharacters_refuse_preparation_and_every_json_text_field() {
        let _lock = lock();
        let source = BASE.replace(
            "[resolver \"primary\"]",
            "name = \"bad \u{fffe}\"\n[resolver \"primary\"]",
        );
        assert_eq!(
            Catalog::prepare(&resolved(&source), &td_crypto::Provider).unwrap_err(),
            PreparationError::Adapter(Error::Invalid)
        );
        for value in ["\u{fdd0}", "\u{ffff}", "\u{10ffff}"] {
            let mut bytes = [0; 64];
            let mut out = TextBuffer::new(&mut bytes);
            assert_eq!(quoted(&mut out, value), Err(Error::Invalid));
            assert_eq!(out.len(), 0);
        }
    }
}
