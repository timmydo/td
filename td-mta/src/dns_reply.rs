//! Bounded DNS answer extraction after network peer admission.
use crate::dns_wire::{Error, Name, Question, WireQuestion, MAX_PACKET_BYTES};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const MAX_RECORDS: usize = 256;
pub const MAX_ALIASES: usize = 8;
pub const MAX_ADDRESSES: usize = 16;

#[derive(Debug, Eq, PartialEq)]
pub enum Reply {
    Truncated,
    ResponseCode(u8),
    NoData,
    Alias {
        name: Name,
        hops: usize,
        ttl: u32,
        path: Path,
    },
    Addresses(Addresses),
}
/// Complete local chain, including the queried name and final target.
#[derive(Debug, Eq, PartialEq)]
pub struct Path {
    names: [Option<Name>; MAX_ALIASES + 1],
}
impl Path {
    pub fn names(&self) -> impl Iterator<Item = &Name> {
        self.names.iter().flatten()
    }
    fn empty() -> Self {
        Self {
            names: [const { None }; MAX_ALIASES + 1],
        }
    }
}
impl Reply {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Alias { path, .. } => Some(path),
            Self::Addresses(addresses) => Some(&addresses.path),
            _ => None,
        }
    }
}
#[derive(Debug, Eq, PartialEq)]
pub struct Addresses {
    values: [IpAddr; MAX_ADDRESSES],
    count: usize,
    ttl: u32,
    path: Path,
}
impl Addresses {
    pub fn values(&self) -> &[IpAddr] {
        self.values.get(..self.count).unwrap_or_default()
    }
    pub const fn ttl(&self) -> u32 {
        self.ttl
    }
}
#[derive(Clone, Copy, Default)]
pub(crate) struct Record {
    owner: u16,
    kind: u16,
    class: u16,
    data: u16,
    length: u16,
    ttl: u32,
}
fn word(packet: &[u8], at: usize) -> Result<u16, Error> {
    let end = at.checked_add(2).ok_or(Error::Invalid)?;
    Ok(u16::from_be_bytes(
        packet
            .get(at..end)
            .ok_or(Error::Invalid)?
            .try_into()
            .map_err(|_| Error::Invalid)?,
    ))
}
pub(crate) fn value(packet: &[u8], record: Record) -> Result<&[u8], Error> {
    let start = usize::from(record.data);
    packet
        .get(start..start + usize::from(record.length))
        .ok_or(Error::Invalid)
}
fn alias(packet: &[u8], record: Record) -> Result<Name, Error> {
    target(packet, record, 0)
}

pub(crate) fn target(packet: &[u8], record: Record, skip: usize) -> Result<Name, Error> {
    let expected = usize::from(record.length)
        .checked_sub(skip)
        .ok_or(Error::Invalid)?;
    let (name, consumed) = Name::read(packet, usize::from(record.data) + skip)?;
    if consumed != expected {
        return Err(Error::Invalid);
    }
    Ok(name)
}
fn record(packet: &[u8], at: usize) -> Result<(Record, usize), Error> {
    let (_, used) = Name::read(packet, at)?;
    let fixed = at.checked_add(used).ok_or(Error::Invalid)?;
    let data = fixed.checked_add(10).ok_or(Error::Invalid)?;
    let kind = word(packet, fixed)?;
    // Our question has no EDNS; do not ignore an unsolicited extended RCODE.
    if kind == 41 {
        return Err(Error::Invalid);
    }
    let class = word(packet, fixed + 2)?;
    let ttl = u32::from_be_bytes(
        packet
            .get(fixed + 4..fixed + 8)
            .ok_or(Error::Invalid)?
            .try_into()
            .map_err(|_| Error::Invalid)?,
    );
    let length = word(packet, fixed + 8)?;
    let end = data
        .checked_add(usize::from(length))
        .ok_or(Error::Invalid)?;
    packet.get(data..end).ok_or(Error::Invalid)?;
    let record = Record {
        owner: u16::try_from(at).map_err(|_| Error::Invalid)?,
        kind,
        class,
        data: u16::try_from(data).map_err(|_| Error::Invalid)?,
        length,
        ttl: if ttl & 0x8000_0000 != 0 { 0 } else { ttl },
    };
    if class == 1 {
        match kind {
            1 if length != 4 => return Err(Error::Invalid),
            28 if length != 16 => return Err(Error::Invalid),
            5 => {
                alias(packet, record)?;
            }
            _ => {}
        }
    }
    Ok((record, end))
}

/// Match the transaction and complete question, then extract only answer-section
/// IN addresses reached from that question. No authentication or cache admission.
/// A TC reply needs a complete matching question but may have a partial RR tail.
/// Non-TC replies require complete bounded framing, including unused sections.
/// Supply the original chain's remaining allowance, at most MAX_ALIASES.
/// Subtract Alias::hops before a continuation; successful addresses also obey
/// this allowance, even when the terminal packet contains further aliases.
pub fn read(packet: &[u8], question: &Question, remaining_aliases: usize) -> Result<Reply, Error> {
    if remaining_aliases > MAX_ALIASES {
        return Err(Error::Capacity);
    }
    match parse(packet, question.wire(), |answers| {
        extract(packet, question, answers, remaining_aliases)
    })? {
        Message::Truncated => Ok(Reply::Truncated),
        Message::ResponseCode(code) => Ok(Reply::ResponseCode(code)),
        Message::Answer(reply) => Ok(reply),
    }
}

pub(crate) enum Message<T> {
    Truncated,
    ResponseCode(u8),
    Answer(T),
}

pub(crate) fn parse<T>(
    packet: &[u8],
    question: WireQuestion<'_>,
    extract: impl FnOnce(&[Record]) -> Result<T, Error>,
) -> Result<Message<T>, Error> {
    if packet.len() > MAX_PACKET_BYTES {
        return Err(Error::Capacity);
    }
    if packet.len() < 12 || word(packet, 0)? != question.id {
        return Err(Error::Invalid);
    }
    let flags = word(packet, 2)?;
    if flags & 0xf840 != 0x8000 || word(packet, 4)? != 1 {
        return Err(Error::Invalid);
    }
    let (name, used) = Name::read(packet, 12)?;
    let mut position = 12 + used;
    if &name != question.name
        || word(packet, position)? != question.kind
        || word(packet, position + 2)? != 1
    {
        return Err(Error::Invalid);
    }
    position += 4;
    if flags & 0x0200 != 0 {
        return Ok(Message::Truncated);
    }
    let answers = usize::from(word(packet, 6)?);
    let total = answers + usize::from(word(packet, 8)?) + usize::from(word(packet, 10)?);
    if total > MAX_RECORDS {
        return Err(Error::Capacity);
    }
    let mut records = [Record::default(); MAX_RECORDS];
    for index in 0..total {
        let (rr, end) = record(packet, position)?;
        if index < answers {
            *records.get_mut(index).ok_or(Error::Capacity)? = rr;
        }
        position = end;
    }
    if position != packet.len() {
        return Err(Error::Invalid);
    }
    if flags & 0x000f != 0 {
        return Ok(Message::ResponseCode((flags & 0x000f) as u8));
    }
    extract(records.get(..answers).ok_or(Error::Capacity)?).map(Message::Answer)
}
fn extract(
    packet: &[u8],
    question: &Question,
    records: &[Record],
    remaining_aliases: usize,
) -> Result<Reply, Error> {
    let mut values = [IpAddr::V4(Ipv4Addr::UNSPECIFIED); MAX_ADDRESSES];
    let mut count = 0;
    let (path, hops, ttl) = walk(packet, question.wire(), records, remaining_aliases, |rr| {
        let addr = address(packet, rr)?;
        if !values.get(..count).ok_or(Error::Capacity)?.contains(&addr) {
            *values.get_mut(count).ok_or(Error::Capacity)? = addr;
            count += 1;
        }
        Ok(())
    })?;
    if count != 0 {
        Ok(Reply::Addresses(Addresses {
            values,
            count,
            ttl,
            path,
        }))
    } else if hops != 0 {
        Ok(Reply::Alias {
            name: *path.names().last().ok_or(Error::Invalid)?,
            hops,
            ttl,
            path,
        })
    } else {
        Ok(Reply::NoData)
    }
}

pub(crate) fn address(packet: &[u8], rr: Record) -> Result<IpAddr, Error> {
    let bytes = value(packet, rr)?;
    match rr.kind {
        1 => Ok(IpAddr::V4(Ipv4Addr::from(
            <[u8; 4]>::try_from(bytes).map_err(|_| Error::Invalid)?,
        ))),
        28 => Ok(IpAddr::V6(Ipv6Addr::from(
            <[u8; 16]>::try_from(bytes).map_err(|_| Error::Invalid)?,
        ))),
        _ => Err(Error::Invalid),
    }
}

pub(crate) fn walk(
    packet: &[u8],
    question: WireQuestion<'_>,
    records: &[Record],
    remaining_aliases: usize,
    mut visit: impl FnMut(Record) -> Result<(), Error>,
) -> Result<(Path, usize, u32), Error> {
    let mut path = Path::empty();
    let mut current = *question.name;
    let mut ttl = u32::MAX;
    for hops in 0..=remaining_aliases {
        if path.names().any(|name| name == &current) {
            return Err(Error::Invalid);
        }
        *path.names.get_mut(hops).ok_or(Error::Capacity)? = Some(current);
        let mut next = None;
        let mut has_data = false;
        for rr in records.iter().copied().filter(|rr| rr.class == 1) {
            if Name::read(packet, usize::from(rr.owner))?.0 != current {
                continue;
            }
            if rr.kind == 5 {
                let target = alias(packet, rr)?;
                if next.as_ref().is_some_and(|prior| prior != &target) {
                    return Err(Error::Invalid);
                }
                next = Some(target);
                ttl = ttl.min(rr.ttl);
            } else if rr.kind == question.kind {
                has_data = true;
                ttl = ttl.min(rr.ttl);
                visit(rr)?;
            } else if matches!(rr.kind, 1 | 28) {
                has_data = true;
            }
        }
        match next {
            Some(_) if has_data => return Err(Error::Invalid),
            Some(target) => current = target,
            None => return Ok((path, hops, ttl)),
        }
    }
    Err(Error::Capacity)
}

const _: () = assert!(std::mem::size_of::<Record>() <= 16);
const _: () = assert!(std::mem::size_of::<Reply>() <= 3 * 1024);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::dns_wire::{AddressType, MAX_QUERY_BYTES};
    fn read(packet: &[u8], question: &Question) -> Result<Reply, Error> {
        super::read(packet, question, MAX_ALIASES)
    }
    fn query(kind: AddressType) -> Question {
        Question::new("relay.test", kind, 0x1234).unwrap()
    }
    fn rr(name: &str, kind: u16, ttl: u32, data: &[u8]) -> Vec<u8> {
        let mut bytes = Name::from_host(name).unwrap().wire().to_vec();
        bytes.extend_from_slice(&kind.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&ttl.to_be_bytes());
        bytes.extend_from_slice(&(data.len() as u16).to_be_bytes());
        bytes.extend_from_slice(data);
        bytes
    }
    fn cname(owner: &str, name: &str, ttl: u32) -> Vec<u8> {
        rr(owner, 5, ttl, Name::from_host(name).unwrap().wire())
    }
    fn packet(
        question: &Question,
        answers: &[Vec<u8>],
        authority: &[Vec<u8>],
        additional: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut cell = [0; MAX_QUERY_BYTES];
        let mut bytes = question.write(&mut cell).unwrap().to_vec();
        bytes[2] |= 0x80;
        for (offset, rows) in [(6, answers), (8, authority), (10, additional)] {
            bytes[offset..offset + 2].copy_from_slice(&(rows.len() as u16).to_be_bytes());
            for row in rows {
                bytes.extend_from_slice(row);
            }
        }
        bytes
    }
    fn addresses(reply: Reply) -> Addresses {
        match reply {
            Reply::Addresses(value) => value,
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn exact_address_families_deduplication_and_minimum_ttl() {
        let q = query(AddressType::A);
        let reply = read(
            &packet(
                &q,
                &[
                    rr("other.test", 1, 1, &[9, 9, 9, 9]),
                    rr("relay.test", 1, 90, &[192, 0, 2, 1]),
                    rr("RELAY.TEST", 1, 20, &[192, 0, 2, 2]),
                    rr("relay.test", 1, 10, &[192, 0, 2, 1]),
                ],
                &[],
                &[],
            ),
            &q,
        )
        .unwrap();
        let result = addresses(reply);
        assert_eq!(
            result.values(),
            [IpAddr::from([192, 0, 2, 1]), IpAddr::from([192, 0, 2, 2])]
        );
        assert_eq!(result.ttl(), 10);
        let q = query(AddressType::Aaaa);
        let result = addresses(
            read(
                &packet(
                    &q,
                    &[rr(
                        "relay.test",
                        28,
                        0x8000_0001,
                        &Ipv6Addr::LOCALHOST.octets(),
                    )],
                    &[],
                    &[],
                ),
                &q,
            )
            .unwrap(),
        );
        assert_eq!(result.values(), [IpAddr::V6(Ipv6Addr::LOCALHOST)]);
        assert_eq!(result.ttl(), 0);
    }
    #[test]
    fn cname_order_is_irrelevant_and_limits_ttl() {
        let q = query(AddressType::A);
        let result = addresses(
            read(
                &packet(
                    &q,
                    &[
                        rr("last.test", 1, 80, &[192, 0, 2, 1]),
                        cname("middle.test", "last.test", 90),
                        cname("relay.test", "middle.test", 30),
                        cname("RELAY.TEST", "MIDDLE.TEST", 20),
                    ],
                    &[],
                    &[],
                ),
                &q,
            )
            .unwrap(),
        );
        assert_eq!(result.ttl(), 20);
        assert_eq!(result.values(), [IpAddr::from([192, 0, 2, 1])]);
        assert_eq!(
            read(
                &packet(&q, &[cname("relay.test", "next.test", 5)], &[], &[]),
                &q
            ),
            Ok(Reply::Alias {
                name: Name::from_host("next.test").unwrap(),
                hops: 1,
                ttl: 5,
                path: Path {
                    names: std::array::from_fn(|i| match i {
                        0 => Some(Name::from_host("relay.test").unwrap()),
                        1 => Some(Name::from_host("next.test").unwrap()),
                        _ => None,
                    })
                }
            })
        );
        for rows in [
            vec![cname("relay.test", "relay.test", 1)],
            vec![
                cname("relay.test", "next.test", 1),
                cname("next.test", "relay.test", 1),
            ],
            vec![
                cname("relay.test", "one.test", 1),
                cname("relay.test", "two.test", 1),
            ],
            vec![
                cname("relay.test", "next.test", 1),
                rr("relay.test", 28, 1, &[0; 16]),
            ],
        ] {
            assert_eq!(read(&packet(&q, &rows, &[], &[]), &q), Err(Error::Invalid));
        }
    }
    #[test]
    fn answer_section_only_and_complete_framing() {
        let q = query(AddressType::A);
        let row = rr("relay.test", 1, 10, &[192, 0, 2, 1]);
        assert_eq!(
            read(
                &packet(
                    &q,
                    &[],
                    std::slice::from_ref(&row),
                    std::slice::from_ref(&row)
                ),
                &q
            ),
            Ok(Reply::NoData)
        );
        let bytes = packet(&q, &[row], &[], &[]);
        for size in 0..bytes.len() {
            assert!(read(&bytes[..size], &q).is_err(), "{size}");
        }
        let mut tail = bytes.clone();
        tail.push(0);
        assert_eq!(read(&tail, &q), Err(Error::Invalid));
        for (kind, payload) in [
            (1, vec![0; 3]),
            (28, vec![0; 17]),
            (5, vec![0, 0]),
            (41, vec![]),
        ] {
            assert!(read(
                &packet(&q, &[], &[], &[rr("relay.test", kind, 10, &payload)]),
                &q
            )
            .is_err());
        }
    }
    #[test]
    fn transaction_question_and_header_match_precede_truncation() {
        let q = query(AddressType::A);
        let mut bytes = packet(&q, &[], &[], &[]);
        bytes[2] |= 2;
        bytes[6..8].copy_from_slice(&300u16.to_be_bytes());
        bytes.extend_from_slice(b"partial");
        assert_eq!(read(&bytes, &q), Ok(Reply::Truncated));
        for (offset, mask) in [
            (0, 1),
            (2, 0x80),
            (2, 8),
            (3, 0x40),
            (5, 1),
            (13, 1),
            (24, 1),
            (25, 1),
            (26, 1),
            (27, 1),
        ] {
            let mut bad = bytes.clone();
            bad[offset] ^= mask;
            assert_eq!(read(&bad, &q), Err(Error::Invalid), "{offset}");
        }
        let mut failed = packet(&q, &[], &[], &[]);
        failed[3] |= 3;
        assert_eq!(read(&failed, &q), Ok(Reply::ResponseCode(3)));
        let mut dnssec_bits = packet(&q, &[], &[], &[]);
        dnssec_bits[3] |= 0x30;
        assert_eq!(read(&dnssec_bits, &q), Ok(Reply::NoData));
    }
    #[test]
    fn fixed_record_address_and_alias_limits() {
        let q = query(AddressType::A);
        for count in [16, 17] {
            let rows: Vec<_> = (0..count)
                .map(|n| rr("relay.test", 1, 10, &[192, 0, 2, n]))
                .collect();
            let result = read(&packet(&q, &rows, &[], &[]), &q);
            if count == 16 {
                assert_eq!(addresses(result.unwrap()).values().len(), 16);
            } else {
                assert_eq!(result, Err(Error::Capacity));
            }
        }
        for count in [256, 257] {
            let rows = vec![rr("other.test", 16, 10, &[]); count];
            assert_eq!(
                read(&packet(&q, &[], &rows, &[]), &q),
                if count == 256 {
                    Ok(Reply::NoData)
                } else {
                    Err(Error::Capacity)
                }
            );
        }
        for count in [8, 9] {
            let mut rows = Vec::new();
            let mut owner = "relay.test".to_owned();
            for n in 0..count {
                let next = format!("n{n}.test");
                rows.push(cname(&owner, &next, 10));
                owner = next;
            }
            rows.push(rr(&owner, 1, 20, &[192, 0, 2, 1]));
            let result = read(&packet(&q, &rows, &[], &[]), &q);
            if count == 8 {
                assert_eq!(addresses(result.unwrap()).ttl(), 10);
            } else {
                assert_eq!(result, Err(Error::Capacity));
            }
        }
    }
    #[test]
    fn continuation_keeps_original_alias_allowance_through_terminal_answer() {
        let first = query(AddressType::A);
        let initial = packet(&first, &[cname("relay.test", "next.test", 5)], &[], &[]);
        let Reply::Alias { name, hops, .. } = super::read(&initial, &first, MAX_ALIASES).unwrap()
        else {
            panic!("continuation");
        };
        let next = Question::new("next.test", AddressType::A, 0x5678).unwrap();
        assert_eq!(&name, next.name());
        assert_eq!(hops, 1);
        let remaining = MAX_ALIASES.checked_sub(hops).unwrap();
        for count in [7, 8] {
            let mut rows = Vec::new();
            let mut owner = "next.test".to_owned();
            for n in 0..count {
                let target = format!("n{n}.test");
                rows.push(cname(&owner, &target, 10));
                owner = target;
            }
            rows.push(rr(&owner, 1, 20, &[192, 0, 2, 1]));
            let response = packet(&next, &rows, &[], &[]);
            let result = super::read(&response, &next, remaining);
            if count == 7 {
                assert_eq!(
                    addresses(result.unwrap()).values(),
                    [IpAddr::from([192, 0, 2, 1])]
                );
            } else {
                assert_eq!(result, Err(Error::Capacity));
            }
        }
        let direct = packet(&next, &[rr("next.test", 1, 20, &[192, 0, 2, 1])], &[], &[]);
        assert!(matches!(
            super::read(&direct, &next, 0),
            Ok(Reply::Addresses(_))
        ));
        assert_eq!(super::read(&initial, &first, 0), Err(Error::Capacity));
        assert_eq!(
            super::read(&direct, &next, MAX_ALIASES + 1),
            Err(Error::Capacity)
        );
    }

    #[test]
    fn compressed_owner_and_alias_target_keep_exact_rdata_extent() {
        let q = query(AddressType::A);
        let mut bytes = packet(&q, &[], &[], &[]);
        bytes[7] = 2;
        bytes
            .extend_from_slice(b"\xc0\x0c\x00\x05\x00\x01\x00\x00\x00\x0a\x00\x07\x04next\xc0\x12");
        bytes.extend_from_slice(&rr("next.test", 1, 20, &[192, 0, 2, 1]));
        assert_eq!(addresses(read(&bytes, &q).unwrap()).ttl(), 10);
    }
}
