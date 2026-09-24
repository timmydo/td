//! Canonical visible-identity preimages; no digest, authorization or publication.
use crate::ids::IdentityId;
use std::fmt;

pub const MAX_IDENTITIES: usize = 64;
pub const MAX_ADDRESSES: usize = 16;
pub const MAX_NAME_BYTES: usize = 4096;
pub const MAX_EMAIL_BYTES: usize = 254;
pub const MAX_SIGNATURE_BYTES: usize = 16 * 1024;
pub const MAX_PREIMAGE_BYTES: usize = 192 * 1024;
const PREFIX: &[u8] = b"td-mta-identities-v1\0";

#[derive(Clone, Copy)]
pub struct Address<'a> {
    pub name: Option<&'a str>,
    pub email: &'a str,
}
#[derive(Clone, Copy)]
pub struct Identity<'a> {
    pub id: IdentityId,
    pub name: &'a str,
    pub email: &'a str,
    pub reply_to: Option<&'a [Address<'a>]>,
    pub bcc: Option<&'a [Address<'a>]>,
    pub text_signature: &'a str,
    pub html_signature: &'a str,
}
impl fmt::Debug for Address<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Address(<redacted>)")
    }
}
impl fmt::Debug for Identity<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Identity(<redacted>)")
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    IdentityCount,
    AddressCount,
    TextLimit,
    PreimageLimit,
    IdentityOrder,
}
impl Error {
    pub const fn name(self) -> &'static str {
        match self {
            Self::IdentityCount => "identity_preimage_count",
            Self::AddressCount => "identity_preimage_address_count",
            Self::TextLimit => "identity_preimage_text_limit",
            Self::PreimageLimit => "identity_preimage_size_limit",
            Self::IdentityOrder => "identity_preimage_order",
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
impl std::error::Error for Error {}

#[derive(Eq, PartialEq)]
pub enum WriteError<E> {
    Invalid(Error),
    Sink(E),
}
impl<E> fmt::Debug for WriteError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(e) => f.debug_tuple("Invalid").field(e).finish(),
            Self::Sink(_) => f.write_str("Sink(<redacted>)"),
        }
    }
}
impl<E> fmt::Display for WriteError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(e) => e.fmt(f),
            Self::Sink(_) => f.write_str("identity preimage sink failed"),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for WriteError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(e) => Some(e),
            Self::Sink(e) => Some(e),
        }
    }
}
fn length(value: usize) -> Result<[u8; 4], Error> {
    Ok(u32::try_from(value)
        .map_err(|_| Error::PreimageLimit)?
        .to_le_bytes())
}
fn string_size(value: &str, maximum: usize) -> Result<usize, Error> {
    if value.len() > maximum {
        return Err(Error::TextLimit);
    }
    4usize.checked_add(value.len()).ok_or(Error::PreimageLimit)
}
fn add(total: &mut usize, size: usize) -> Result<(), Error> {
    let next = total.checked_add(size).ok_or(Error::PreimageLimit)?;
    if next > MAX_PREIMAGE_BYTES {
        return Err(Error::PreimageLimit);
    }
    *total = next;
    Ok(())
}
fn addresses_size(total: &mut usize, addresses: Option<&[Address<'_>]>) -> Result<(), Error> {
    add(total, 1)?;
    let Some(addresses) = addresses else {
        return Ok(());
    };
    if addresses.len() > MAX_ADDRESSES {
        return Err(Error::AddressCount);
    }
    add(total, 4)?;
    for address in addresses {
        add(total, 1)?;
        if let Some(name) = address.name {
            add(total, string_size(name, MAX_NAME_BYTES)?)?;
        }
        add(total, string_size(address.email, MAX_EMAIL_BYTES)?)?;
    }
    Ok(())
}
/// Checks representation bounds/order only. Mailbox syntax, account scope and
/// defaults belong to the configuration loader. Empty snapshots are encodable.
pub fn encoded_len(identities: &[Identity<'_>]) -> Result<usize, Error> {
    if identities.len() > MAX_IDENTITIES {
        return Err(Error::IdentityCount);
    }
    let mut total = PREFIX.len();
    add(&mut total, 4)?;
    let mut previous = None;
    for identity in identities {
        if previous.is_some_and(|id| id >= identity.id.as_bytes()) {
            return Err(Error::IdentityOrder);
        }
        previous = Some(identity.id.as_bytes());
        add(&mut total, identity.id.as_bytes().len())?;
        add(&mut total, string_size(identity.name, MAX_NAME_BYTES)?)?;
        add(&mut total, string_size(identity.email, MAX_EMAIL_BYTES)?)?;
        addresses_size(&mut total, identity.reply_to)?;
        addresses_size(&mut total, identity.bcc)?;
        add(
            &mut total,
            string_size(identity.text_signature, MAX_SIGNATURE_BYTES)?,
        )?;
        add(
            &mut total,
            string_size(identity.html_signature, MAX_SIGNATURE_BYTES)?,
        )?;
        add(&mut total, 1)?; // V1 mayDelete is always false.
    }
    Ok(total)
}
fn emit<E>(
    sink: &mut impl FnMut(&[u8]) -> Result<(), E>,
    bytes: &[u8],
) -> Result<(), WriteError<E>> {
    sink(bytes).map_err(WriteError::Sink)
}
fn emit_string<E>(
    sink: &mut impl FnMut(&[u8]) -> Result<(), E>,
    value: &str,
) -> Result<(), WriteError<E>> {
    emit(sink, &length(value.len()).map_err(WriteError::Invalid)?)?;
    if !value.is_empty() {
        emit(sink, value.as_bytes())?;
    }
    Ok(())
}
fn emit_addresses<E>(
    sink: &mut impl FnMut(&[u8]) -> Result<(), E>,
    addresses: Option<&[Address<'_>]>,
) -> Result<(), WriteError<E>> {
    let Some(addresses) = addresses else {
        return emit(sink, &[0]);
    };
    emit(sink, &[1])?;
    emit(sink, &length(addresses.len()).map_err(WriteError::Invalid)?)?;
    for address in addresses {
        if let Some(name) = address.name {
            emit(sink, &[1])?;
            emit_string(sink, name)?;
        } else {
            emit(sink, &[0])?;
        }
        emit_string(sink, address.email)?;
    }
    Ok(())
}
/// Validate the whole immutable input before the first sink call. Sink failure
/// may leave a prefix in the sink; discard that digest/output, never publish it.
/// The trusted sink owns its allocation, failure and panic behavior.
pub fn write_preimage<E>(
    identities: &[Identity<'_>],
    mut sink: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<usize, WriteError<E>> {
    let size = encoded_len(identities).map_err(WriteError::Invalid)?;
    emit(&mut sink, PREFIX)?;
    emit(
        &mut sink,
        &length(identities.len()).map_err(WriteError::Invalid)?,
    )?;
    for identity in identities {
        emit(&mut sink, identity.id.as_bytes())?;
        emit_string(&mut sink, identity.name)?;
        emit_string(&mut sink, identity.email)?;
        emit_addresses(&mut sink, identity.reply_to)?;
        emit_addresses(&mut sink, identity.bcc)?;
        emit_string(&mut sink, identity.text_signature)?;
        emit_string(&mut sink, identity.html_signature)?;
        emit(&mut sink, &[0])?;
    }
    Ok(size)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn identity(id: u8) -> Identity<'static> {
        Identity {
            id: IdentityId::from_bytes([id; 16]),
            name: "",
            email: "",
            reply_to: None,
            bcc: None,
            text_signature: "",
            html_signature: "",
        }
    }
    fn hex(input: &str) -> Vec<u8> {
        let (pairs, remainder) = input.as_bytes().as_chunks::<2>();
        assert!(remainder.is_empty());
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    fn encoded(input: &[Identity<'_>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let count = write_preimage(input, |chunk| {
            assert!(!chunk.is_empty());
            bytes.extend_from_slice(chunk);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(count, bytes.len());
        assert_eq!(encoded_len(input), Ok(count));
        bytes
    }
    #[test]
    fn committed_empty_and_single_identity_oracles() {
        assert_eq!(
            encoded(&[]),
            hex("74642d6d74612d6964656e7469746965732d76310000000000")
        );
        let item = Identity {
            name: "Me",
            email: "me@example.org",
            bcc: Some(&[]),
            ..identity(0x11)
        };
        assert_eq!(encoded(&[item]), hex("74642d6d74612d6964656e7469746965732d7631000100000011111111111111111111111111111111020000004d650e0000006d65406578616d706c652e6f7267000100000000000000000000000000"));
    }
    #[test]
    fn all_visible_fields_have_a_literal_oracle() {
        let reply = [
            Address {
                name: None,
                email: "a@b",
            },
            Address {
                name: Some(""),
                email: "c@d",
            },
        ];
        let bcc = [Address {
            name: Some("N"),
            email: "e@f",
        }];
        let item = Identity {
            name: "é",
            email: "x@y",
            reply_to: Some(&reply),
            bcc: Some(&bcc),
            text_signature: "t\n",
            html_signature: "<b>",
            ..identity(0x22)
        };
        assert_eq!(
            encoded(&[item]),
            hex(concat!(
                "74642d6d74612d6964656e7469746965732d76310001000000",
                "22222222222222222222222222222222",
                "02000000c3a9030000007840790102000000",
                "0003000000614062010000000003000000634064",
                "010100000001010000004e03000000654066",
                "02000000740a030000003c623e00"
            ))
        );
        let mut changed = item;
        changed.reply_to = None;
        assert_ne!(encoded(&[item]), encoded(&[changed]));
        changed.reply_to = Some(&[]);
        assert_ne!(encoded(&[item]), encoded(&[changed]));
        let swapped = [reply[1], reply[0]];
        changed.reply_to = Some(&swapped);
        assert_ne!(encoded(&[item]), encoded(&[changed]));
        assert_ne!(
            encoded(&[identity(1)]),
            encoded(&[Identity {
                bcc: Some(&[]),
                ..identity(1)
            }])
        );
    }
    #[test]
    fn multiple_identities_follow_raw_byte_order() {
        let low = Identity {
            id: IdentityId::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            ..identity(0)
        };
        let high = Identity {
            id: IdentityId::from_bytes([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            ..identity(0)
        };
        assert_eq!(
            encoded(&[low, high]),
            hex(concat!(
                "74642d6d74612d6964656e7469746965732d76310002000000",
                "00000000000000000000000000000001",
                "00000000000000000000000000000000000000",
                "01000000000000000000000000000000",
                "00000000000000000000000000000000000000"
            ))
        );
        assert_eq!(encoded_len(&[high, low]), Err(Error::IdentityOrder));
        let middle = Identity {
            id: IdentityId::from_bytes([0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]),
            ..identity(0)
        };
        assert!(encoded_len(&[low, middle, high]).is_ok());
        assert_eq!(
            encoded_len(&[low, middle, middle, high]),
            Err(Error::IdentityOrder)
        );
    }
    #[test]
    fn order_duplicates_and_counts_are_checked_before_any_output() {
        for items in [
            vec![identity(2), identity(1)],
            vec![identity(1), identity(1)],
        ] {
            let mut calls = 0;
            assert_eq!(
                write_preimage(&items, |_| {
                    calls += 1;
                    Ok::<_, ()>(())
                }),
                Err(WriteError::Invalid(Error::IdentityOrder))
            );
            assert_eq!(calls, 0);
        }
        let mut items: Vec<_> = (0..=64).map(identity).collect();
        assert_eq!(encoded_len(&items), Err(Error::IdentityCount));
        items.pop();
        assert!(encoded_len(&items).is_ok());
        let addresses = [Address {
            name: None,
            email: "a@b",
        }; MAX_ADDRESSES + 1];
        for bcc in [false, true] {
            let mut item = identity(1);
            if bcc {
                item.bcc = Some(&addresses);
            } else {
                item.reply_to = Some(&addresses);
            }
            assert_eq!(encoded_len(&[item]), Err(Error::AddressCount));
            if bcc {
                item.bcc = Some(&addresses[..MAX_ADDRESSES]);
            } else {
                item.reply_to = Some(&addresses[..MAX_ADDRESSES]);
            }
            assert!(encoded_len(&[item]).is_ok());
        }
    }
    #[test]
    fn each_string_ceiling_and_aggregate_boundary_are_enforced() {
        for field in 0..6 {
            let maximum = match field {
                0 | 2 => MAX_NAME_BYTES,
                1 | 3 => MAX_EMAIL_BYTES,
                _ => MAX_SIGNATURE_BYTES,
            };
            let bytes = "x".repeat(maximum + 1);
            for accepted in [true, false] {
                let value = if accepted { &bytes[..maximum] } else { &bytes };
                let addresses = [Address {
                    name: Some(if field == 2 { value } else { "" }),
                    email: if field == 3 { value } else { "" },
                }];
                let mut item = identity(2);
                match field {
                    0 => item.name = value,
                    1 => item.email = value,
                    2 | 3 => {
                        item.reply_to = Some(&addresses);
                        item.bcc = Some(&addresses);
                    }
                    4 => item.text_signature = value,
                    _ => item.html_signature = value,
                }
                let mut calls = 0;
                let result = write_preimage(&[identity(1), item], |_| {
                    calls += 1;
                    Ok::<_, ()>(())
                });
                if accepted {
                    assert!(result.is_ok());
                } else {
                    assert_eq!(result, Err(WriteError::Invalid(Error::TextLimit)));
                    assert_eq!(calls, 0);
                }
            }
        }
        let signature = "s".repeat(MAX_SIGNATURE_BYTES);
        let mut items: Vec<_> = (0..6)
            .map(|id| Identity {
                text_signature: &signature,
                html_signature: &signature,
                ..identity(id)
            })
            .collect();
        assert_eq!(encoded_len(&items), Err(Error::PreimageLimit));
        items[5].html_signature = "";
        let short = encoded_len(&items).unwrap();
        let remaining = MAX_PREIMAGE_BYTES - short;
        items[5].html_signature = &signature[..remaining];
        assert_eq!(encoded(&items).len(), MAX_PREIMAGE_BYTES);
        items[5].html_signature = &signature[..remaining + 1];
        let mut calls = 0;
        assert_eq!(
            write_preimage(&items, |_| {
                calls += 1;
                Ok::<_, ()>(())
            }),
            Err(WriteError::Invalid(Error::PreimageLimit))
        );
        assert_eq!(calls, 0);
    }
    #[test]
    fn sink_failure_stops_immediately_and_can_only_leave_a_prefix() {
        let reply = [
            Address {
                name: None,
                email: "a@b",
            },
            Address {
                name: Some(""),
                email: "c@d",
            },
        ];
        let bcc = [Address {
            name: Some("N"),
            email: "e@f",
        }];
        let complete = Identity {
            name: "é",
            email: "x@y",
            reply_to: Some(&reply),
            bcc: Some(&bcc),
            text_signature: "t\n",
            html_signature: "<b>",
            ..identity(0x22)
        };
        for item in [identity(1), complete] {
            let items = [item];
            let expected = encoded(&items);
            let mut chunks = 0;
            write_preimage(&items, |_| {
                chunks += 1;
                Ok::<_, ()>(())
            })
            .unwrap();
            for fail_at in 0..chunks {
                let mut calls = 0;
                let mut output = Vec::new();
                let result = write_preimage(&items, |bytes| {
                    calls += 1;
                    if calls == fail_at + 1 {
                        return Err(17);
                    }
                    output.extend_from_slice(bytes);
                    Ok(())
                });
                assert_eq!(result, Err(WriteError::Sink(17)));
                assert_eq!(calls, fail_at + 1);
                assert!(expected.starts_with(&output));
            }
        }
    }
    #[test]
    fn debug_and_fixed_errors_do_not_echo_visible_text() {
        let marker = "private_fixture";
        let address = Address {
            name: Some(marker),
            email: marker,
        };
        let item = Identity {
            name: marker,
            ..identity(1)
        };
        assert!(!format!("{address:?} {item:?}").contains(marker));
        for (error, name) in [
            (Error::IdentityCount, "identity_preimage_count"),
            (Error::AddressCount, "identity_preimage_address_count"),
            (Error::TextLimit, "identity_preimage_text_limit"),
            (Error::PreimageLimit, "identity_preimage_size_limit"),
            (Error::IdentityOrder, "identity_preimage_order"),
        ] {
            assert_eq!(error.name(), name);
            assert_eq!(error.to_string(), name);
        }
        assert!(!WriteError::Sink(marker).to_string().contains(marker));
        assert!(!format!("{:?}", WriteError::Sink(marker)).contains(marker));
    }
}
