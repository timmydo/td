//! Bounded visible-identity assembly for a trusted control-worker sink.
use super::{identity, materialize};
use crate::ids::IdentityId;
use std::{fmt, mem::size_of};

const ADDRESS_COUNT: usize = super::identities::MAX_ADDRESS_CELLS;
const EMPTY_ADDRESS: identity::Address<'static> = identity::Address {
    name: None,
    email: "",
};
const EMPTY_IDENTITY: identity::Identity<'static> = identity::Identity {
    id: IdentityId::from_bytes([0; 16]),
    name: "",
    email: "",
    reply_to: None,
    bcc: None,
    text_signature: "",
    html_signature: "",
};
#[derive(Clone, Copy)]
struct ListRange {
    start: usize,
    length: usize,
    present: bool,
}
impl ListRange {
    const EMPTY: Self = Self {
        start: 0,
        length: 0,
        present: false,
    };
    fn borrow<'a>(
        self,
        addresses: &'a [identity::Address<'a>],
    ) -> Option<Option<&'a [identity::Address<'a>]>> {
        if !self.present {
            return Some(None);
        }
        addresses
            .get(self.start..self.start.checked_add(self.length)?)
            .map(Some)
    }
}
pub(crate) const WORKSPACE_BYTES: usize = ADDRESS_COUNT * size_of::<identity::Address<'static>>()
    + identity::MAX_IDENTITIES * size_of::<identity::Identity<'static>>()
    + 2 * identity::MAX_IDENTITIES * size_of::<ListRange>();
const _: [(); 1] = [(); (WORKSPACE_BYTES <= 80 * 1024) as usize];

pub enum Error<E> {
    Invariant,
    Invalid(identity::Error),
    Sink(E),
}
impl<E> Error<E> {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Invariant => "config_identity_view_invariant",
            Self::Invalid(error) => error.name(),
            Self::Sink(_) => "config_identity_preimage_sink",
        }
    }
}
impl<E> fmt::Debug for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
impl<E> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
impl<E> std::error::Error for Error<E> {}

/// Validate and stream canonical visible-identity bytes without retaining them.
/// The sink must remain private to configuration work until M05/M07 checks
/// complete. A sink error or panic may leave a prefix; discard that output or
/// digest. Sink allocations, blocking and panic behavior are caller obligations.
/// This function grants no file trust, authorization or publication authority.
pub fn write<E>(
    resolved: &materialize::ResolvedText,
    sink: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<usize, Error<E>> {
    let records = resolved
        .candidate()
        .identities()
        .map_err(|_| Error::Invariant)?;
    let count = records.len();
    if count > identity::MAX_IDENTITIES {
        return Err(Error::Invariant);
    }
    let mut addresses = [EMPTY_ADDRESS; ADDRESS_COUNT];
    let mut ranges = [[ListRange::EMPTY; 2]; identity::MAX_IDENTITIES];
    let mut used = 0usize;
    for index in 0..count {
        let record = records
            .identity(index)
            .map_err(|_| Error::Invariant)?
            .ok_or(Error::Invariant)?;
        for (kind, list) in [record.reply_to, record.bcc].into_iter().enumerate() {
            let Some(list) = list else { continue };
            let start = used;
            let mut length = 0usize;
            for address in list {
                if length >= identity::MAX_ADDRESSES {
                    return Err(Error::Invariant);
                }
                let address = address.map_err(|_| Error::Invariant)?;
                *addresses.get_mut(used).ok_or(Error::Invariant)? = identity::Address {
                    name: address.name,
                    email: address.email,
                };
                length = length.checked_add(1).ok_or(Error::Invariant)?;
                used = used.checked_add(1).ok_or(Error::Invariant)?;
            }
            *ranges
                .get_mut(index)
                .and_then(|row| row.get_mut(kind))
                .ok_or(Error::Invariant)? = ListRange {
                start,
                length,
                present: true,
            };
        }
    }
    let addresses = addresses.get(..used).ok_or(Error::Invariant)?;
    let mut identities = [EMPTY_IDENTITY; identity::MAX_IDENTITIES];
    for index in 0..count {
        let record = records
            .identity(index)
            .map_err(|_| Error::Invariant)?
            .ok_or(Error::Invariant)?;
        let signatures = resolved
            .signatures(index)
            .map_err(|_| Error::Invariant)?
            .ok_or(Error::Invariant)?;
        let ranges = ranges.get(index).ok_or(Error::Invariant)?;
        let reply_to = ranges
            .first()
            .ok_or(Error::Invariant)?
            .borrow(addresses)
            .ok_or(Error::Invariant)?;
        let bcc = ranges
            .get(1)
            .ok_or(Error::Invariant)?
            .borrow(addresses)
            .ok_or(Error::Invariant)?;
        *identities.get_mut(index).ok_or(Error::Invariant)? = identity::Identity {
            id: record.id,
            name: record.name,
            email: record.email,
            reply_to,
            bcc,
            text_signature: signatures.text,
            html_signature: signatures.html,
        };
    }
    identity::write_preimage(identities.get(..count).ok_or(Error::Invariant)?, sink).map_err(
        |error| match error {
            identity::WriteError::Invalid(error) => Error::Invalid(error),
            identity::WriteError::Sink(error) => Error::Sink(error),
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::{inputs, load, stanza::Pending, storage::Storage, stream, text};
    const BASE: &str = r#"version = 1
[server]
hostname = "mail.example.test"
jmap_origin = "https://jmap.example.test"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "private-user"
[domain "example.test"]
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "relay.example.test"
port = 465
username = "private-relay"
password_file = "/private-password"
[certificate "public"]
mode = "files"
chain_file = "/private-chain"
key_file = "/private-key"
[listener "smtp"]
kind = "direct_smtp"
bind = "0.0.0.0:25"
server_name = "mail.example.test"
certificate = "public"
session_limit = 1
per_peer_limit = 1
[listener "https"]
kind = "https"
bind = "0.0.0.0:443"
certificate = "public"
"#;
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn resolved(source: &str) -> materialize::ResolvedText {
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let loaded = load::read(
            Storage::try_new().unwrap(),
            &mut Pending::new(),
            &mut scratch,
            &mut source.as_bytes(),
        )
        .unwrap();
        materialize::read_text(loaded, &mut scratch, |reference| {
            let bytes: &'static [u8] = match reference.target() {
                inputs::Target::TextSignature(_) => b"T\n",
                inputs::Target::HtmlSignature(_) => b"<b>H</b>",
                inputs::Target::RelayPassword => b"private-password-excluded",
                _ => panic!("provider input opened"),
            };
            Ok::<_, ()>(bytes)
        })
        .unwrap()
    }
    fn bytes(resolved: &materialize::ResolvedText) -> Vec<u8> {
        let mut bytes = Vec::new();
        let count = write(resolved, |chunk| {
            bytes.extend_from_slice(chunk);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(count, bytes.len());
        bytes
    }
    #[test]
    fn complete_view_has_an_independent_literal_preimage() {
        let _lock = lock();
        let source = BASE.replace("[resolver \"primary\"]", "name = \"Main\"\nreply_to = true\nbcc = true\ntext_signature_file = \"/text\"\nhtml_signature_file = \"/html\"\n[resolver \"primary\"]") + r#"[identity_address "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
kind = "reply_to"
email = "reply@example.test"
[identity_address "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
kind = "reply_to"
name = ""
email = "second@example.test"
[identity_address "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
kind = "reply_to"
name = "Reply Name"
email = "named@example.test"
"#;
        let resolved = resolved(&source);
        let actual = bytes(&resolved);
        let mut expected = b"td-mta-identities-v1\0\x01\0\0\0".to_vec();
        expected.extend_from_slice(&[0xbb; 16]);
        expected.extend_from_slice(b"\x04\0\0\0Main\x11\0\0\0main@example.test");
        expected.extend_from_slice(b"\x01\x03\0\0\0\0\x12\0\0\0reply@example.test");
        expected.extend_from_slice(b"\x01\0\0\0\0\x13\0\0\0second@example.test");
        expected.extend_from_slice(b"\x01\x0a\0\0\0Reply Name\x12\0\0\0named@example.test");
        expected.extend_from_slice(b"\x01\0\0\0\0\x02\0\0\0T\n\x08\0\0\0<b>H</b>\0");
        assert_eq!(actual, expected);
        assert!(!actual
            .windows(b"private-password-excluded".len())
            .any(|window| window == b"private-password-excluded"));
        assert_eq!(
            resolved.relay_password().unwrap(),
            "private-password-excluded"
        );
    }
    #[test]
    fn preimage_limit_is_independent_of_successful_text_materialization() {
        let _lock = lock();
        let name = "x".repeat(3020);
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
            source.push_str(&format!(
                r#"[identity "{n:032x}"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
name = "{name}"
email = "main@example.test"
"#
            ));
        }
        let resolved = resolved(&source);
        let mut calls = 0;
        let error = write(&resolved, |_| {
            calls += 1;
            Ok::<_, ()>(())
        })
        .unwrap_err();
        assert_eq!(calls, 0);
        assert!(matches!(
            error,
            Error::Invalid(identity::Error::PreimageLimit)
        ));
    }
    #[test]
    fn sink_failure_stops_and_redacts_payload_and_source() {
        let _lock = lock();
        let resolved = resolved(BASE);
        let mut calls = 0;
        let mut prefix = Vec::new();
        let error = write(&resolved, |chunk| {
            calls += 1;
            if calls == 3 {
                return Err(std::io::Error::other("private sink detail"));
            }
            prefix.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap_err();
        assert_eq!(calls, 3);
        assert_eq!(prefix, b"td-mta-identities-v1\0\x01\0\0\0");
        assert_eq!(
            format!("{error:?} {error}"),
            "config_identity_preimage_sink config_identity_preimage_sink"
        );
        assert!(std::error::Error::source(&error).is_none());
        let Error::Sink(inner) = error else {
            panic!("wrong error")
        };
        assert_eq!(inner.to_string(), "private sink detail");
    }
    #[test]
    fn full_address_tables_preserve_raw_id_and_declared_list_order() {
        let _lock = lock();
        let mut source = BASE
            .replace(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "00000000000000000000000000000001",
            )
            .replace(
                "[resolver \"primary\"]",
                "reply_to = true\nbcc = true\n[resolver \"primary\"]",
            );
        for n in (2..=64).rev() {
            source.push_str(&format!(
                r#"[identity "{n:032x}"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
reply_to = true
bcc = true
"#
            ));
        }
        for index in (0..16).rev() {
            for kind in ["reply_to", "bcc"] {
                for n in (1..=64).rev() {
                    source.push_str(&format!(
                        r#"[identity_address "{n:032x}"]
kind = "{kind}"
email = "{kind}{n:02}-{index:02}@example.test"
"#
                    ));
                    if (index % 2 == 0) == (kind == "reply_to") {
                        source.push_str("name = \"\"\n");
                    }
                }
            }
        }
        let resolved = resolved(&source);
        let encoded = bytes(&resolved);
        let mut input = encoded.as_slice();
        fn take<'a>(input: &mut &'a [u8], count: usize) -> &'a [u8] {
            let (head, tail) = input.split_at(count);
            *input = tail;
            head
        }
        fn count(input: &mut &[u8]) -> usize {
            u32::from_le_bytes(take(input, 4).try_into().unwrap()) as usize
        }
        fn string<'a>(input: &mut &'a [u8]) -> &'a [u8] {
            let n = count(input);
            take(input, n)
        }
        assert_eq!(
            take(&mut input, b"td-mta-identities-v1\0".len()),
            b"td-mta-identities-v1\0"
        );
        assert_eq!(count(&mut input), 64);
        for n in 1..=64u128 {
            assert_eq!(take(&mut input, 16), n.to_be_bytes());
            assert_eq!(string(&mut input), b"");
            assert_eq!(string(&mut input), b"main@example.test");
            for kind in ["reply_to", "bcc"] {
                assert_eq!(take(&mut input, 1), [1]);
                assert_eq!(count(&mut input), 16);
                for index in (0..16).rev() {
                    if (index % 2 == 0) == (kind == "reply_to") {
                        assert_eq!(take(&mut input, 1), [1]);
                        assert_eq!(string(&mut input), b"");
                    } else {
                        assert_eq!(take(&mut input, 1), [0]);
                    }
                    assert_eq!(
                        string(&mut input),
                        format!("{kind}{n:02}-{index:02}@example.test").as_bytes()
                    );
                }
            }
            assert_eq!(string(&mut input), b"");
            assert_eq!(string(&mut input), b"");
            assert_eq!(take(&mut input, 1), [0]);
        }
        assert!(input.is_empty());
    }
    #[test]
    fn absent_lists_and_present_empty_lists_have_distinct_bytes() {
        let _lock = lock();
        let mut prefix = b"td-mta-identities-v1\0\x01\0\0\0".to_vec();
        prefix.extend_from_slice(&[0xbb; 16]);
        prefix.extend_from_slice(b"\0\0\0\0\x11\0\0\0main@example.test");
        let mut absent = prefix.clone();
        absent.extend_from_slice(&[0; 11]); // two null lists, two empty signatures, mayDelete.
        assert_eq!(bytes(&resolved(BASE)), absent);
        let mut present = prefix;
        present.extend_from_slice(b"\x01\0\0\0\0\x01\0\0\0\0");
        present.extend_from_slice(&[0; 9]);
        let source = BASE.replace(
            "[resolver \"primary\"]",
            "reply_to = true\nbcc = true\n[resolver \"primary\"]",
        );
        assert_eq!(bytes(&resolved(&source)), present);
        assert_ne!(absent, present);
    }
}
