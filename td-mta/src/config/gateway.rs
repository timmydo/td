//! Structural gateway policies; pins and prefixes alone grant no TLS authority.
use super::{endpoint::Prefix, syntax::Location, text, values};
use std::{fmt, num::NonZeroU64};
pub const MAX_GATEWAYS: usize = 16;
pub const MAX_PEERS_PER_GATEWAY: usize = 8;
pub const MAX_PEERS: usize = MAX_GATEWAYS * MAX_PEERS_PER_GATEWAY;
const NO_PEER: u8 = u8::MAX;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    ForeignArena,
    Profile,
    Path,
    Pin,
    SamePins,
    Prefix,
    DuplicateGateway,
    DuplicatePeer,
    MissingGateway,
    Text,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_gateway_capacity",
            Self::ForeignArena => "config_gateway_foreign_arena",
            Self::Profile => "config_gateway_profile",
            Self::Path => "config_gateway_path",
            Self::Pin => "config_gateway_pin",
            Self::SamePins => "config_gateway_same_pins",
            Self::Prefix => "config_gateway_prefix",
            Self::DuplicateGateway => "config_gateway_duplicate",
            Self::DuplicatePeer => "config_gateway_duplicate_peer",
            Self::MissingGateway => "config_gateway_missing",
            Self::Text => "config_gateway_text",
            Self::Invariant => "config_gateway_invariant",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub location: Option<Location>,
    pub previous: Option<Location>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.name())?;
        if let Some(at) = self.location {
            write!(f, " at line {}, byte column {}", at.line, at.column)?;
        }
        if let Some(at) = self.previous {
            write!(
                f,
                " (previously configured at line {}, byte column {})",
                at.line, at.column
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {}
fn err(code: Code, at: Option<Location>) -> Error {
    Error {
        code,
        location: at,
        previous: None,
    }
}
fn duplicate(code: Code, at: Location, previous: Location) -> Error {
    Error {
        code,
        location: Some(at),
        previous: Some(previous),
    }
}
fn invariant() -> Error {
    err(Code::Invariant, None)
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct LeafPin([u8; 32]);
impl fmt::Debug for LeafPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LeafPin(<redacted>)")
    }
}
impl LeafPin {
    pub fn parse(input: &str) -> Result<Self, Error> {
        if input.len() != 64 {
            return Err(err(Code::Pin, None));
        }
        let mut bytes = [0; 32];
        let (pairs, _) = input.as_bytes().as_chunks::<2>();
        for (out, &[high, low]) in bytes.iter_mut().zip(pairs) {
            *out = (nibble(high)? << 4) | nibble(low)?;
        }
        Ok(Self(bytes))
    }
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
fn nibble(byte: u8) -> Result<u8, Error> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(err(Code::Pin, None)),
    }
}
#[derive(Clone, Copy)]
pub struct Slot {
    name: text::Span,
    ca: text::Span,
    pin: LeafPin,
    next: LeafPin,
    peers: [u8; MAX_PEERS_PER_GATEWAY],
    count: u8,
    has_next: bool,
    declared: bool,
    first: Option<Location>,
    declaration: Option<Location>,
}
impl Slot {
    pub const EMPTY: Self = Self {
        name: text::Span::EMPTY,
        ca: text::Span::EMPTY,
        pin: LeafPin([0; 32]),
        next: LeafPin([0; 32]),
        peers: [NO_PEER; MAX_PEERS_PER_GATEWAY],
        count: 0,
        has_next: false,
        declared: false,
        first: None,
        declaration: None,
    };
}
#[derive(Clone, Copy)]
pub struct PeerSlot {
    prefix: Option<Prefix>,
    location: Option<Location>,
}
impl PeerSlot {
    pub const EMPTY: Self = Self {
        prefix: None,
        location: None,
    };
}
const _: [(); 1] = [(); (std::mem::size_of::<Slot>() <= 256) as usize];
const _: [(); 1] = [(); (std::mem::size_of::<PeerSlot>() <= 32) as usize];
const _: [(); 1] = [(); (MAX_PEERS <= u8::MAX as usize) as usize];
pub struct Input<'a> {
    pub ca_file: &'a str,
    pub client_cert_sha256: &'a str,
    pub next_client_cert_sha256: Option<&'a str>,
}
impl fmt::Debug for Input<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GatewayInput(<redacted>)")
    }
}
pub struct Builder<'a> {
    slots: &'a mut [Slot],
    peers: &'a mut [PeerSlot],
    count: usize,
    peer_count: usize,
    owner: NonZeroU64,
    failure: Option<Error>,
}
impl fmt::Debug for Builder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GatewayBuilder(<redacted>)")
    }
}
impl<'a> Builder<'a> {
    pub fn new(
        arena: &text::Builder<'_>,
        slots: &'a mut [Slot],
        peers: &'a mut [PeerSlot],
    ) -> Result<Self, Error> {
        if slots.len() > MAX_GATEWAYS || peers.len() > MAX_PEERS {
            return Err(err(Code::Capacity, None));
        }
        Ok(Self {
            slots,
            peers,
            count: 0,
            peer_count: 0,
            owner: arena.owner(),
            failure: None,
        })
    }
    fn operation<T>(
        &mut self,
        arena: &mut text::Builder<'_>,
        at: Location,
        action: impl FnOnce(&mut Self, &mut text::Builder<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = if self.owner != arena.owner() {
            Err(err(Code::ForeignArena, Some(at)))
        } else {
            action(self, arena)
        };
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    fn find(
        &self,
        arena: &text::Builder<'_>,
        name: &str,
        at: Location,
    ) -> Result<Option<usize>, Error> {
        values::profile_name(name).map_err(|_| err(Code::Profile, Some(at)))?;
        let view = arena.borrowed_view().map_err(|_| invariant())?;
        for (index, slot) in self
            .slots
            .get(..self.count)
            .ok_or_else(invariant)?
            .iter()
            .enumerate()
        {
            if read(view, self.owner, slot.name)? == name {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }
    fn insert(
        &mut self,
        arena: &mut text::Builder<'_>,
        name: &str,
        at: Location,
    ) -> Result<usize, Error> {
        if self.count >= self.slots.len() {
            return Err(err(Code::Capacity, Some(at)));
        }
        let name = store(arena, name, at)?;
        let index = self.count;
        *self.slots.get_mut(index).ok_or_else(invariant)? = Slot {
            name,
            first: Some(at),
            ..Slot::EMPTY
        };
        self.count = self.count.checked_add(1).ok_or_else(invariant)?;
        Ok(index)
    }
    pub fn gateway(
        &mut self,
        arena: &mut text::Builder<'_>,
        name: &str,
        input: Input<'_>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            let existing = this.find(arena, name, at)?;
            if let Some(index) = existing {
                let slot = this.slots.get(index).ok_or_else(invariant)?;
                if slot.declared {
                    return Err(duplicate(
                        Code::DuplicateGateway,
                        at,
                        slot.declaration.ok_or_else(invariant)?,
                    ));
                }
            }
            values::absolute_path(input.ca_file).map_err(|_| err(Code::Path, Some(at)))?;
            let pin =
                LeafPin::parse(input.client_cert_sha256).map_err(|_| err(Code::Pin, Some(at)))?;
            let next = input
                .next_client_cert_sha256
                .map(LeafPin::parse)
                .transpose()
                .map_err(|_| err(Code::Pin, Some(at)))?;
            if next == Some(pin) {
                return Err(err(Code::SamePins, Some(at)));
            }
            let index = match existing {
                Some(index) => index,
                None => this.insert(arena, name, at)?,
            };
            let slot = this.slots.get_mut(index).ok_or_else(invariant)?;
            let ca = store(arena, input.ca_file, at)?;
            slot.ca = ca;
            slot.pin = pin;
            slot.next = next.unwrap_or(LeafPin([0; 32]));
            slot.has_next = next.is_some();
            slot.declared = true;
            slot.declaration = Some(at);
            Ok(())
        })
    }
    pub fn peer(
        &mut self,
        arena: &mut text::Builder<'_>,
        name: &str,
        network: &str,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            let existing = this.find(arena, name, at)?;
            let prefix = Prefix::parse(network).map_err(|_| err(Code::Prefix, Some(at)))?;
            let index = match existing {
                Some(index) => index,
                None => this.insert(arena, name, at)?,
            };
            let slot = this.slots.get_mut(index).ok_or_else(invariant)?;
            for global_index in slot
                .peers
                .get(..usize::from(slot.count))
                .ok_or_else(invariant)?
            {
                let old = this
                    .peers
                    .get(usize::from(*global_index))
                    .ok_or_else(invariant)?;
                if old.prefix == Some(prefix) {
                    return Err(duplicate(
                        Code::DuplicatePeer,
                        at,
                        old.location.ok_or_else(invariant)?,
                    ));
                }
            }
            if usize::from(slot.count) >= MAX_PEERS_PER_GATEWAY
                || this.peer_count >= this.peers.len()
            {
                return Err(err(Code::Capacity, Some(at)));
            }
            let peer_index = u8::try_from(this.peer_count).map_err(|_| invariant())?;
            *this.peers.get_mut(this.peer_count).ok_or_else(invariant)? = PeerSlot {
                prefix: Some(prefix),
                location: Some(at),
            };
            *slot
                .peers
                .get_mut(usize::from(slot.count))
                .ok_or_else(invariant)? = peer_index;
            slot.count = slot.count.checked_add(1).ok_or_else(invariant)?;
            this.peer_count = this.peer_count.checked_add(1).ok_or_else(invariant)?;
            Ok(())
        })
    }
    pub fn finish(self) -> Result<Records<'a>, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let slots = self.slots.get(..self.count).ok_or_else(invariant)?;
        for slot in slots {
            if !slot.declared {
                return Err(err(Code::MissingGateway, slot.first));
            }
        }
        Ok(Records {
            slots,
            peers: self.peers.get(..self.peer_count).ok_or_else(invariant)?,
            owner: self.owner,
        })
    }
}
fn store(arena: &mut text::Builder<'_>, input: &str, at: Location) -> Result<text::Span, Error> {
    let handle = arena
        .append(input.as_bytes())
        .map_err(|_| err(Code::Text, Some(at)))?;
    arena.compact(handle).map_err(|_| invariant())
}
fn read(text: text::View<'_>, owner: NonZeroU64, span: text::Span) -> Result<&str, Error> {
    std::str::from_utf8(text.read_span(owner, span).map_err(|_| invariant())?)
        .map_err(|_| invariant())
}
pub struct Records<'a> {
    slots: &'a [Slot],
    peers: &'a [PeerSlot],
    owner: NonZeroU64,
}
impl fmt::Debug for Records<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GatewayRecords(<redacted>)")
    }
}
impl Records<'_> {
    pub fn view_live<'s, 't>(&'s self, text: &'t text::Builder<'_>) -> Result<View<'s, 't>, Error> {
        self.view(text.borrowed_view().map_err(|_| invariant())?)
    }
    pub fn view<'s, 't>(&'s self, text: text::View<'t>) -> Result<View<'s, 't>, Error> {
        if self.owner != text.owner() {
            return Err(err(Code::ForeignArena, None));
        }
        Ok(View {
            slots: self.slots,
            peers: self.peers,
            owner: self.owner,
            text,
        })
    }
}
#[derive(Clone, Copy)]
pub struct View<'s, 't> {
    slots: &'s [Slot],
    peers: &'s [PeerSlot],
    owner: NonZeroU64,
    text: text::View<'t>,
}
impl fmt::Debug for View<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GatewayView(<redacted>)")
    }
}
pub struct Gateway<'s, 't> {
    pub name: &'t str,
    pub ca_file: &'t str,
    pub client_cert_sha256: LeafPin,
    pub next_client_cert_sha256: Option<LeafPin>,
    pub location: Location,
    slot: &'s Slot,
    peers: &'s [PeerSlot],
}
impl fmt::Debug for Gateway<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Gateway(<redacted>)")
    }
}
impl<'s, 't> View<'s, 't> {
    pub fn len(self) -> usize {
        self.slots.len()
    }
    pub fn is_empty(self) -> bool {
        self.slots.is_empty()
    }
    pub fn gateway(self, index: usize) -> Result<Option<Gateway<'s, 't>>, Error> {
        let Some(slot) = self.slots.get(index) else {
            return Ok(None);
        };
        Ok(Some(Gateway {
            name: read(self.text, self.owner, slot.name)?,
            ca_file: read(self.text, self.owner, slot.ca)?,
            client_cert_sha256: slot.pin,
            next_client_cert_sha256: slot.has_next.then_some(slot.next),
            location: slot.declaration.ok_or_else(invariant)?,
            slot,
            peers: self.peers,
        }))
    }
}
impl Gateway<'_, '_> {
    pub fn peer_count(&self) -> usize {
        usize::from(self.slot.count)
    }
    pub fn peer(&self, index: usize) -> Result<Option<Prefix>, Error> {
        if index >= self.peer_count() {
            return Ok(None);
        }
        let global_index = self.slot.peers.get(index).ok_or_else(invariant)?;
        self.peers
            .get(usize::from(*global_index))
            .and_then(|p| p.prefix)
            .map(Some)
            .ok_or_else(invariant)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    const PIN: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const NEXT: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn at(line: u32) -> Location {
        Location {
            line: NonZeroU32::new(line).unwrap(),
            column: 2,
        }
    }
    fn input() -> Input<'static> {
        Input {
            ca_file: "/does-not-exist/private.pem",
            client_cert_sha256: PIN,
            next_client_cert_sha256: Some(NEXT),
        }
    }
    struct Storage {
        bytes: Vec<u8>,
        slots: Vec<Slot>,
        peers: Vec<PeerSlot>,
    }
    impl Storage {
        fn new() -> Self {
            Self {
                bytes: vec![0; text::MAX_BYTES],
                slots: vec![Slot::EMPTY; MAX_GATEWAYS],
                peers: vec![PeerSlot::EMPTY; MAX_PEERS],
            }
        }
    }
    #[test]
    fn forward_rows_rotation_and_overlapping_prefixes_retain_association() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
        b.peer(&mut arena, "west", "2001:db8::/32", at(1)).unwrap();
        b.peer(&mut arena, "east", "192.0.2.0/24", at(2)).unwrap();
        b.gateway(&mut arena, "east", input(), at(3)).unwrap();
        b.peer(&mut arena, "east", "192.0.2.0/25", at(4)).unwrap();
        b.gateway(
            &mut arena,
            "west",
            Input {
                next_client_cert_sha256: None,
                ..input()
            },
            at(5),
        )
        .unwrap();
        b.gateway(&mut arena, "staged", input(), at(6)).unwrap();
        let records = b.finish().unwrap();
        {
            let view = records.view_live(&arena).unwrap();
            assert_eq!(view.len(), 3);
            assert!(!view.is_empty());
            let west = view.gateway(0).unwrap().unwrap();
            let east = view.gateway(1).unwrap().unwrap();
            assert_eq!(west.name, "west");
            assert_eq!(west.location, at(5));
            assert!(west.next_client_cert_sha256.is_none());
            assert_eq!(
                west.peer(0).unwrap(),
                Some(Prefix::parse("2001:db8::/32").unwrap())
            );
            assert_eq!(east.name, "east");
            assert_eq!(east.ca_file, "/does-not-exist/private.pem");
            assert_eq!(
                east.client_cert_sha256.as_bytes(),
                &std::array::from_fn(|i| i as u8)
            );
            assert_eq!(east.next_client_cert_sha256.unwrap().as_bytes(), &[255; 32]);
            assert_eq!(east.peer_count(), 2);
            assert_eq!(
                east.peer(1).unwrap(),
                Some(Prefix::parse("192.0.2.0/25").unwrap())
            );
            assert!(east
                .peer(0)
                .unwrap()
                .unwrap()
                .contains("::ffff:192.0.2.4".parse().unwrap())
                .unwrap());
            assert!(east.peer(usize::MAX).unwrap().is_none());
            assert!(view.gateway(usize::MAX).unwrap().is_none());
            let staged = view.gateway(2).unwrap().unwrap();
            assert_eq!(staged.peer_count(), 0);
            assert!(staged.peer(0).unwrap().is_none());
            assert!(east.peer(east.peer_count()).unwrap().is_none());
        }
        arena.append(b"protected material later").unwrap();
        assert_eq!(
            records
                .view(arena.freeze().unwrap())
                .unwrap()
                .gateway(1)
                .unwrap()
                .unwrap()
                .location,
            at(3)
        );
    }
    #[test]
    fn pins_require_exact_lowercase_encoding_and_rotation_difference() {
        let _lock = lock();
        assert_eq!(
            LeafPin::parse(PIN).unwrap().as_bytes(),
            &std::array::from_fn(|i| i as u8)
        );
        for bad in [
            "".to_string(),
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "é".repeat(32),
            "g".repeat(64),
        ] {
            assert_eq!(LeafPin::parse(&bad).unwrap_err().code, Code::Pin);
        }
        for (current, next, code) in [
            (PIN, Some(PIN), Code::SamePins),
            ("x", None, Code::Pin),
            (PIN, Some("x"), Code::Pin),
        ] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            let e = b
                .gateway(
                    &mut arena,
                    "g",
                    Input {
                        client_cert_sha256: current,
                        next_client_cert_sha256: next,
                        ..input()
                    },
                    at(2),
                )
                .unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(e.location, Some(at(2)));
            assert_eq!(b.finish().unwrap_err(), e);
        }
    }
    #[test]
    fn duplicates_and_missing_forward_declarations_are_located_and_sticky() {
        let _lock = lock();
        for gateway in [false, true] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            let (e, code) = if gateway {
                b.gateway(&mut arena, "g", input(), at(1)).unwrap();
                (
                    b.gateway(&mut arena, "g", input(), at(3)).unwrap_err(),
                    Code::DuplicateGateway,
                )
            } else {
                b.peer(&mut arena, "g", "2001:db8::/32", at(1)).unwrap();
                (
                    b.peer(&mut arena, "g", "2001:0db8:0:0:0:0:0:0/32", at(3))
                        .unwrap_err(),
                    Code::DuplicatePeer,
                )
            };
            assert_eq!(
                (e.code, e.location, e.previous),
                (code, Some(at(3)), Some(at(1)))
            );
            assert_eq!(
                b.peer(&mut arena, "other", "192.0.2.0/24", at(4))
                    .unwrap_err(),
                e
            );
            assert_eq!(b.finish().unwrap_err(), e);
        }
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
        b.peer(&mut arena, "undeclared", "192.0.2.0/24", at(8))
            .unwrap();
        assert_eq!(
            b.finish().unwrap_err(),
            err(Code::MissingGateway, Some(at(8)))
        );
    }
    #[test]
    fn forward_duplicate_coordinates_later_peers_and_validation_order_are_pinned() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        {
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            b.peer(&mut arena, "g", "192.0.2.0/24", at(1)).unwrap();
            b.gateway(&mut arena, "g", input(), at(2)).unwrap();
            let e = b
                .gateway(
                    &mut arena,
                    "g",
                    Input {
                        ca_file: "relative",
                        client_cert_sha256: "bad",
                        ..input()
                    },
                    at(3),
                )
                .unwrap_err();
            assert_eq!((e.code, e.previous), (Code::DuplicateGateway, Some(at(2))));
            assert_eq!(e.to_string(), "config_gateway_duplicate at line 3, byte column 2 (previously configured at line 2, byte column 2)");
        }
        {
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            b.peer(&mut arena, "g", "192.0.2.0/24", at(4)).unwrap();
            b.peer(&mut arena, "g", "2001:db8::/32", at(5)).unwrap();
            let e = b
                .peer(&mut arena, "g", "2001:0db8:0:0:0:0:0:0/32", at(6))
                .unwrap_err();
            assert_eq!((e.code, e.previous), (Code::DuplicatePeer, Some(at(5))));
            assert_eq!(e.to_string(), "config_gateway_duplicate_peer at line 6, byte column 2 (previously configured at line 5, byte column 2)");
        }
        for peer in [false, true] {
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            let e = if peer {
                b.peer(&mut arena, "Invalid", "bad", at(7)).unwrap_err()
            } else {
                b.gateway(
                    &mut arena,
                    "Invalid",
                    Input {
                        client_cert_sha256: "bad",
                        ..input()
                    },
                    at(7),
                )
                .unwrap_err()
            };
            assert_eq!(e.code, Code::Profile);
            assert_eq!(b.finish().unwrap_err(), e);
        }
        let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
        b.peer(&mut arena, "g", "192.0.2.0/24", at(8)).unwrap();
        assert_eq!(
            b.finish().unwrap_err(),
            err(Code::MissingGateway, Some(at(8)))
        );
        assert!(Slot::EMPTY
            .peers
            .iter()
            .all(|i| usize::from(*i) >= MAX_PEERS));
    }
    #[test]
    fn full_layout_and_reused_cells_do_not_retain_previous_generation() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        {
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            for n in 0..16 {
                let name = format!("g{n}");
                for p in 0..8 {
                    b.peer(&mut arena, &name, &format!("192.0.2.{p}/32"), at(1))
                        .unwrap();
                }
                b.gateway(&mut arena, &name, input(), at(2)).unwrap();
            }
            let records = b.finish().unwrap();
            let view = records.view_live(&arena).unwrap();
            assert_eq!(view.len(), 16);
            for n in 0..16 {
                assert_eq!(view.gateway(n).unwrap().unwrap().peer_count(), 8);
            }
        }
        {
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            b.gateway(
                &mut arena,
                "new",
                Input {
                    next_client_cert_sha256: None,
                    ..input()
                },
                at(9),
            )
            .unwrap();
            let records = b.finish().unwrap();
            let view = records.view_live(&arena).unwrap();
            assert_eq!(view.len(), 1);
            let g = view.gateway(0).unwrap().unwrap();
            assert_eq!(g.name, "new");
            assert_eq!(g.peer_count(), 0);
            assert!(g.next_client_cert_sha256.is_none());
        }
        let b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
        let records = b.finish().unwrap();
        assert!(records.view_live(&arena).unwrap().is_empty());
        assert!(std::mem::size_of::<Slot>() <= 256);
        assert!(std::mem::size_of::<PeerSlot>() <= 32);
        assert!(std::mem::size_of::<Builder<'_>>() <= 128);
    }
    #[test]
    fn capacities_invalid_prefixes_and_paths_refuse_without_authority() {
        let _lock = lock();
        for gateway in [false, true] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            if gateway {
                for n in 0..16 {
                    b.gateway(&mut arena, &format!("g{n}"), input(), at(1))
                        .unwrap();
                }
                assert_eq!(
                    b.gateway(&mut arena, "overflow", input(), at(2))
                        .unwrap_err()
                        .code,
                    Code::Capacity
                );
            } else {
                for p in 0..8 {
                    b.peer(&mut arena, "g", &format!("192.0.2.{p}/32"), at(1))
                        .unwrap();
                }
                assert_eq!(
                    b.peer(&mut arena, "g", "192.0.2.8/32", at(2))
                        .unwrap_err()
                        .code,
                    Code::Capacity
                );
            }
            assert_eq!(b.finish().unwrap_err().code, Code::Capacity);
        }
        for bad in [
            "0.0.0.0/0",
            "192.0.2.1/24",
            "192.0.2.0/024",
            "[::]/1",
            "::ffff:192.0.2.0/120",
            "fe80::%1/64",
        ] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            assert_eq!(
                b.peer(&mut arena, "g", bad, at(1)).unwrap_err().code,
                Code::Prefix
            );
        }
        for (name, path, code) in [
            ("Invalid", "/path", Code::Profile),
            ("g", "relative", Code::Path),
            ("g", "/x/../y", Code::Path),
        ] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
            assert_eq!(
                b.gateway(
                    &mut arena,
                    name,
                    Input {
                        ca_file: path,
                        ..input()
                    },
                    at(1)
                )
                .unwrap_err()
                .code,
                code
            );
        }
        let mut bytes = [0; 2];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut slots = [Slot::EMPTY; 1];
        let mut peers = [];
        let mut b = Builder::new(&arena, &mut slots, &mut peers).unwrap();
        let e = b.gateway(&mut arena, "g", input(), at(3)).unwrap_err();
        assert_eq!(e.code, Code::Text);
        assert_eq!(arena.used(), 1);
        assert_eq!(b.finish().unwrap_err(), e);
    }
    #[test]
    fn empty_and_smaller_regions_obey_caps_and_exact_text_bounds() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let records = Builder::new(&arena, &mut [], &mut [])
            .unwrap()
            .finish()
            .unwrap();
        assert!(records.view_live(&arena).unwrap().is_empty());
        let mut too_many_slots = vec![Slot::EMPTY; MAX_GATEWAYS + 1];
        let mut too_many_peers = vec![PeerSlot::EMPTY; MAX_PEERS + 1];
        assert_eq!(
            Builder::new(&arena, &mut too_many_slots, &mut [])
                .unwrap_err()
                .code,
            Code::Capacity
        );
        assert_eq!(
            Builder::new(&arena, &mut [], &mut too_many_peers)
                .unwrap_err()
                .code,
            Code::Capacity
        );
        let mut slots = [Slot::EMPTY; 1];
        let mut peers = [PeerSlot::EMPTY; 1];
        {
            let mut b = Builder::new(&arena, &mut slots, &mut peers).unwrap();
            b.peer(&mut arena, "g", "192.0.2.0/24", at(1)).unwrap();
            let e = b
                .peer(&mut arena, "g", "198.51.100.0/24", at(2))
                .unwrap_err();
            assert_eq!(e.code, Code::Capacity);
            assert_eq!(b.finish().unwrap_err(), e);
        }
        let name = format!("g{}", "a".repeat(63));
        let path = format!("/{}", "p".repeat(4094));
        {
            let mut b = Builder::new(&arena, &mut slots, &mut peers).unwrap();
            b.gateway(
                &mut arena,
                &name,
                Input {
                    ca_file: &path,
                    ..input()
                },
                at(3),
            )
            .unwrap();
            let records = b.finish().unwrap();
            let g = records
                .view_live(&arena)
                .unwrap()
                .gateway(0)
                .unwrap()
                .unwrap();
            assert_eq!(g.name, name);
            assert_eq!(g.ca_file, path);
            assert_eq!(g.peer_count(), 0);
        }
        for (name, path, code) in [
            (format!("{name}a"), path.clone(), Code::Profile),
            (name.clone(), format!("{path}p"), Code::Path),
        ] {
            let mut b = Builder::new(&arena, &mut slots, &mut peers).unwrap();
            let e = b
                .gateway(
                    &mut arena,
                    &name,
                    Input {
                        ca_file: &path,
                        ..input()
                    },
                    at(4),
                )
                .unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(b.finish().unwrap_err(), e);
        }
    }
    #[test]
    fn owner_binding_and_redaction_cover_live_frozen_and_mutating_access() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut foreign_bytes = [0; 4096];
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut foreign = text::Builder::new(&mut foreign_bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots, &mut s.peers).unwrap();
        b.gateway(&mut arena, "private", input(), at(1)).unwrap();
        let records = b.finish().unwrap();
        assert_eq!(
            records.view_live(&foreign).unwrap_err().code,
            Code::ForeignArena
        );
        assert_eq!(
            records
                .view(foreign.borrowed_view().unwrap())
                .unwrap_err()
                .code,
            Code::ForeignArena
        );
        let view = records.view_live(&arena).unwrap();
        let g = view.gateway(0).unwrap().unwrap();
        assert_eq!(format!("{records:?}"), "GatewayRecords(<redacted>)");
        assert_eq!(format!("{view:?}"), "GatewayView(<redacted>)");
        assert_eq!(format!("{g:?}"), "Gateway(<redacted>)");
        assert_eq!(format!("{:?}", input()), "GatewayInput(<redacted>)");
        assert_eq!(format!("{:?}", g.client_cert_sha256), "LeafPin(<redacted>)");
        let mut slots = [Slot::EMPTY; 1];
        let mut peers = [PeerSlot::EMPTY; 1];
        let mut b = Builder::new(&arena, &mut slots, &mut peers).unwrap();
        assert_eq!(format!("{b:?}"), "GatewayBuilder(<redacted>)");
        let e = b
            .peer(&mut foreign, "private", "192.0.2.0/24", at(9))
            .unwrap_err();
        assert_eq!(e.code, Code::ForeignArena);
        assert_eq!(
            b.gateway(&mut arena, "private", input(), at(10))
                .unwrap_err(),
            e
        );
        assert_eq!(b.finish().unwrap_err(), e);
        assert!(!format!("{e:?} {e}").contains("private"));
        assert!(std::error::Error::source(&e).is_none());
    }
}
