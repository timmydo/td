#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::Sequence,
    ids::{AccountId, StoreEpoch},
};
use std::cell::Cell;
const EMAIL: EmailId = EmailId::from_bytes([3; 16]);
fn identity() -> ViewIdentity {
    ViewIdentity {
        account: AccountId::from_bytes([1; 16]),
        epoch: StoreEpoch::from_bytes([2; 16]),
        committed_sequence: Sequence::from_u64(1),
        history_floor: Sequence::default(),
    }
}
struct Fixture<'a> {
    count: u16,
    expired: &'a Cell<bool>,
    absent: bool,
    foreign: bool,
    authentication: Option<fn() -> crate::sender_authentication::Results>,
}
impl Source for Fixture<'_> {
    type Error = Error;
    fn check_snapshot(&mut self) -> Result<(), Error> {
        if self.expired.get() {
            Err(Error::Deadline)
        } else {
            Ok(())
        }
    }
    fn email_metadata(
        &mut self,
        email: EmailId,
        _: &mut [u8],
    ) -> Result<Option<EmailMetadata>, Error> {
        self.check_snapshot()?;
        Ok((!self.absent).then_some(EmailMetadata {
            identity: identity(),
            email,
            blob: BlobId::from_bytes([4; 16]),
            thread: ThreadId::from_bytes([5; 16]),
            size: 32,
            received_at: -1,
        }))
    }
    fn email_mailboxes(
        &mut self,
        email: EmailId,
        after: Option<MailboxId>,
        maximum: usize,
    ) -> Result<EmailValues<MailboxId>, Error> {
        self.check_snapshot()?;
        let mut values: Vec<_> = (0..self.count)
            .map(|n| {
                let mut id = [0; 16];
                id[..2].copy_from_slice(&n.to_be_bytes());
                MailboxId::from_bytes(id)
            })
            .filter(|id| after.is_none_or(|after| *id > after))
            .take(maximum + 1)
            .collect();
        let has_more = values.len() > maximum;
        values.truncate(maximum);
        let mut identity = identity();
        if self.foreign {
            identity.account = AccountId::from_bytes([99; 16]);
        }
        Ok(EmailValues {
            identity,
            email,
            values,
            has_more,
        })
    }
    fn email_keywords(
        &mut self,
        email: EmailId,
        after: Option<&str>,
        maximum: usize,
    ) -> Result<EmailValues<String>, Error> {
        self.check_snapshot()?;
        let mut values: Vec<_> = (0..self.count)
            .map(|n| format!("key{n:03}"))
            .filter(|key| after.is_none_or(|after| key.as_str() > after))
            .take(maximum + 1)
            .collect();
        let has_more = values.len() > maximum;
        values.truncate(maximum);
        Ok(EmailValues {
            identity: identity(),
            email,
            values,
            has_more,
        })
    }
    fn email_authentication(
        &mut self,
        _: EmailId,
    ) -> Result<Option<crate::sender_authentication::Results>, Error> {
        self.check_snapshot()?;
        Ok(self.authentication.map(|make| make()))
    }
}
#[test]
fn complete_projection_pages_sets_and_preserves_wire_fields() {
    let expired = Cell::new(false);
    let mut source = Fixture {
        count: 300,
        expired: &expired,
        absent: false,
        foreign: false,
        authentication: None,
    };
    let mut bytes = Vec::new();
    assert!(
        write_stored(&mut source, EMAIL, Stored::all(), &mut [0; 2048], |chunk| {
            assert!(chunk.len() <= MAX_CHUNK_BYTES);
            bytes.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap()
    );
    let json = td_json::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
    assert_eq!(json.as_obj().unwrap().len(), 7);
    assert_eq!(json["mailboxIds"].as_obj().unwrap().len(), 300);
    assert_eq!(json["keywords"].as_obj().unwrap().len(), 300);
    assert_eq!(
        json["receivedAt"].as_str(),
        Some("1969-12-31T23:59:59.999Z")
    );
    assert_eq!(json["size"].as_u64(), Some(32));
    for (name, prefix) in [("id", "e"), ("blobId", "b"), ("threadId", "t")] {
        assert!(json[name].as_str().unwrap().starts_with(prefix));
    }
}
#[test]
fn explicit_projection_keeps_id_and_absence_emits_nothing() {
    let expired = Cell::new(false);
    let mut source = Fixture {
        count: 1,
        expired: &expired,
        absent: false,
        foreign: false,
        authentication: None,
    };
    for (field, name) in [
        (Field::BlobId, "blobId"),
        (Field::ThreadId, "threadId"),
        (Field::Size, "size"),
        (Field::ReceivedAt, "receivedAt"),
        (Field::MailboxIds, "mailboxIds"),
        (Field::Keywords, "keywords"),
    ] {
        let mut projection = Stored::id_only();
        projection.include(field).unwrap();
        let mut bytes = Vec::new();
        assert!(
            write_stored(&mut source, EMAIL, projection, &mut [0; 2048], |chunk| {
                bytes.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap()
        );
        let json = td_json::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(json.as_obj().unwrap().len(), 2);
        assert!(json.as_obj().unwrap().iter().any(|(key, _)| key == name));
    }
    assert_eq!(
        Stored::id_only().include(Field::Headers),
        Err(Error::Invalid)
    );
    source.absent = true;
    assert!(!write_stored(
        &mut source,
        EMAIL,
        Stored::all(),
        &mut [0; 2048],
        |_| panic!("absent object emitted bytes")
    )
    .unwrap());
}
#[test]
fn sink_failure_expiry_and_cross_snapshot_values_withhold_completion() {
    let expired = Cell::new(false);
    let mut source = Fixture {
        count: 300,
        expired: &expired,
        absent: false,
        foreign: false,
        authentication: None,
    };
    assert_eq!(
        write_stored(&mut source, EMAIL, Stored::all(), &mut [0; 2048], |_| Err(
            Error::Tls
        )),
        Err(Error::Tls)
    );
    assert_eq!(
        write_stored(&mut source, EMAIL, Stored::all(), &mut [0; 2048], |_| {
            expired.set(true);
            Ok(())
        }),
        Err(Error::Deadline)
    );
    expired.set(false);
    source.foreign = true;
    assert!(write_stored(
        &mut source,
        EMAIL,
        Stored::all(),
        &mut [0; 2048],
        |_| Ok(())
    )
    .is_err());
}
#[test]
fn sender_authentication_is_opt_in_and_names_every_recorded_value() {
    use crate::{dkim, dmarc, sender_authentication::Results, spf};
    fn full() -> Results {
        Results {
            spf: spf::Outcome::Softfail,
            spf_domain: Some("bounce.example".into()),
            spf_helo: false,
            dkim: dkim::Report {
                signatures: [
                    dkim::SignatureResult {
                        outcome: dkim::Outcome::Pass,
                        algorithm: Some(td_crypto::DkimAlgorithm::Rsa),
                        domain: Some("example.com".into()),
                        selector: Some("s_1".into()),
                        key_bits: Some(2048),
                    },
                    dkim::SignatureResult {
                        outcome: dkim::Outcome::Permerror,
                        algorithm: None,
                        domain: None,
                        selector: None,
                        key_bits: None,
                    },
                ]
                .into(),
                incomplete: true,
            },
            dmarc: dmarc::Report {
                outcome: dmarc::Outcome::Pass,
                author: Some("mail.example.com".into()),
                policy_domain: Some("example.com".into()),
                policy: Some(dmarc::Requested {
                    disposition: dmarc::Disposition::Quarantine,
                    strict_spf: false,
                    strict_dkim: true,
                }),
                aligned: Some(dmarc::Identifier::Dkim),
            },
        }
    }
    let expired = Cell::new(false);
    let mut source = Fixture {
        count: 1,
        expired: &expired,
        absent: false,
        foreign: false,
        authentication: None,
    };
    let mut projection = Stored::id_only();
    projection.include(Field::SenderAuthentication).unwrap();
    assert!(!Stored::all().selects_authentication());
    let write = |source: &mut Fixture<'_>| {
        let mut bytes = Vec::new();
        write_stored(source, EMAIL, projection, &mut [0; 2048], |chunk| {
            assert!(chunk.len() <= MAX_CHUNK_BYTES);
            td_alloc::extend_from_slice(&mut bytes, chunk).unwrap();
            Ok(())
        })
        .map(|_| td_json::parse(std::str::from_utf8(&bytes).unwrap()).unwrap())
    };
    let json = write(&mut source).unwrap();
    assert!(json["senderAuthentication"].is_null());
    assert_eq!(json.as_obj().unwrap().len(), 2);
    source.authentication = Some(full);
    let json = write(&mut source).unwrap();
    assert_eq!(
        json["senderAuthentication"],
        td_json::json!({
            "spf": {"result": "softfail", "identity": "mailfrom", "domain": "bounce.example"},
            "dkim": {"complete": false, "signatures": [
                {"result": "pass", "algorithm": "rsa-sha256", "keyBits": 2048,
                    "domain": "example.com", "selector": "s_1"},
                {"result": "permerror", "algorithm": null, "keyBits": null,
                    "domain": null, "selector": null}
            ]},
            "dmarc": {"result": "pass", "author": "mail.example.com",
                "policyDomain": "example.com",
                "policy": {"disposition": "quarantine", "strictSpf": false, "strictDkim": true},
                "aligned": "dkim"}
        })
    );
    source.authentication = Some(|| {
        let mut bare = full();
        bare.dkim.signatures.clear();
        bare.dmarc = dmarc::Report::unavailable();
        bare.spf_helo = true;
        bare.spf_domain = None;
        bare
    });
    let json = write(&mut source).unwrap();
    assert_eq!(
        json["senderAuthentication"]["dkim"]["signatures"],
        td_json::json!([])
    );
    assert!(json["senderAuthentication"]["dmarc"]["policy"].is_null());
    assert!(json["senderAuthentication"]["dmarc"]["aligned"].is_null());
    assert_eq!(
        json["senderAuthentication"]["spf"],
        td_json::json!({"result": "softfail", "identity": "helo", "domain": null})
    );
    // Four signatures with the longest names still fit the fixed chunks.
    source.authentication = Some(|| {
        let mut long = full();
        let name = || Some(String::from_utf8(td_alloc::filled(253, b'a').unwrap()).unwrap());
        long.spf_domain = name();
        long.dmarc.author = name();
        long.dmarc.policy_domain = name();
        let row = || dkim::SignatureResult {
            outcome: dkim::Outcome::Temperror,
            algorithm: Some(td_crypto::DkimAlgorithm::Ed25519),
            domain: name(),
            selector: name(),
            key_bits: Some(65535),
        };
        long.dkim.signatures = [row(), row(), row(), row()].into();
        long
    });
    let json = write(&mut source).unwrap();
    let long = &json["senderAuthentication"];
    assert_eq!(long["dkim"]["signatures"].as_array().unwrap().len(), 4);
    assert_eq!(
        long["dkim"]["signatures"][3]["selector"]
            .as_str()
            .unwrap()
            .len(),
        253
    );
    assert_eq!(long["dmarc"]["policyDomain"].as_str().unwrap().len(), 253);
    // A name the field could not state is corruption, never escaped JSON.
    source.authentication = Some(|| {
        let mut forged = full();
        forged.spf_domain = Some("a\"b".into());
        forged
    });
    assert_eq!(write(&mut source).err(), Some(Error::Corrupt));
}
