//! Bounded structural account/identity records. No sending or publication authority.
use super::{identity, syntax::Location, text, values};
use crate::ids::{AccountId, IdentityId};
use std::{
    fmt,
    num::{NonZeroU32, NonZeroU64},
};

pub const MAX_ADDRESS_CELLS: usize = identity::MAX_IDENTITIES * identity::MAX_ADDRESSES * 2;
const NONE: u16 = u16::MAX;
const ORIGIN: Location = Location {
    line: NonZeroU32::MIN,
    column: 1,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    ForeignArena,
    Text,
    Username,
    Name,
    Mailbox,
    Wildcard,
    SignaturePath,
    DuplicateAccount,
    MissingAccount,
    DuplicateIdentity,
    MissingIdentity,
    UnknownIdentity,
    UnknownAccount,
    DisabledList,
    AddressCount,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_identity_capacity",
            Self::ForeignArena => "config_identity_foreign_arena",
            Self::Text => "config_identity_text",
            Self::Username => "config_identity_username",
            Self::Name => "config_identity_name",
            Self::Mailbox => "config_identity_mailbox",
            Self::Wildcard => "config_identity_wildcard",
            Self::SignaturePath => "config_identity_signature_path",
            Self::DuplicateAccount => "config_identity_duplicate_account",
            Self::MissingAccount => "config_identity_missing_account",
            Self::DuplicateIdentity => "config_identity_duplicate",
            Self::MissingIdentity => "config_identity_missing",
            Self::UnknownIdentity => "config_identity_unknown",
            Self::UnknownAccount => "config_identity_unknown_account",
            Self::DisabledList => "config_identity_disabled_list",
            Self::AddressCount => "config_identity_address_count",
            Self::Invariant => "config_identity_invariant",
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
        if let Some(previous) = self.previous {
            let label = if self.code == Code::Capacity {
                "earlier unresolved reference"
            } else {
                "previously declared"
            };
            write!(
                f,
                " ({label} at line {}, byte column {})",
                previous.line, previous.column
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
fn invariant() -> Error {
    err(Code::Invariant, None)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum List {
    ReplyTo,
    Bcc,
}
impl List {
    fn index(self) -> usize {
        match self {
            Self::ReplyTo => 0,
            Self::Bcc => 1,
        }
    }
}
#[derive(Clone, Copy)]
struct Chain {
    first: u16,
    last: u16,
    count: u8,
}
impl Chain {
    const EMPTY: Self = Self {
        first: NONE,
        last: NONE,
        count: 0,
    };
}
#[derive(Clone, Copy)]
pub struct IdentitySlot {
    id: IdentityId,
    account: AccountId,
    name: text::Span,
    email: text::Span,
    text_path: text::Span,
    html_path: text::Span,
    chains: [Chain; 2],
    location: Location,
    declared: bool,
    reply: bool,
    bcc: bool,
    has_text: bool,
    has_html: bool,
}
impl IdentitySlot {
    pub const EMPTY: Self = Self {
        id: IdentityId::from_bytes([0; 16]),
        account: AccountId::from_bytes([0; 16]),
        name: text::Span::EMPTY,
        email: text::Span::EMPTY,
        text_path: text::Span::EMPTY,
        html_path: text::Span::EMPTY,
        chains: [Chain::EMPTY; 2],
        location: ORIGIN,
        declared: false,
        reply: false,
        bcc: false,
        has_text: false,
        has_html: false,
    };
}
#[derive(Clone, Copy)]
pub struct AddressSlot {
    name: text::Span,
    email: text::Span,
    next: u16,
    location: Location,
    has_name: bool,
}
impl AddressSlot {
    pub const EMPTY: Self = Self {
        name: text::Span::EMPTY,
        email: text::Span::EMPTY,
        next: NONE,
        location: ORIGIN,
        has_name: false,
    };
}
// Reserve two future eight-byte resolved signature spans inside each cell.
const _: [(); 1] = [(); (std::mem::size_of::<IdentitySlot>() + 16 <= 128) as usize];
const _: [(); 1] = [(); (std::mem::size_of::<AddressSlot>() <= 32) as usize];
const _: [(); 1] = [(); (MAX_ADDRESS_CELLS < u16::MAX as usize) as usize];
#[derive(Clone, Copy)]
struct Account {
    id: AccountId,
    username: text::Span,
    name: text::Span,
    location: Location,
}
/// Fields already bound to one complete stanza; defaults are explicit here.
pub struct IdentityInput<'a> {
    pub id: IdentityId,
    pub account: AccountId,
    pub name: &'a str,
    pub email: &'a str,
    pub reply_to: bool,
    pub bcc: bool,
    pub text_signature_file: Option<&'a str>,
    pub html_signature_file: Option<&'a str>,
}
pub struct AddressInput<'a> {
    pub identity: IdentityId,
    pub list: List,
    pub name: Option<&'a str>,
    pub email: &'a str,
}
impl fmt::Debug for IdentityInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IdentityInput(<redacted>)")
    }
}
impl fmt::Debug for AddressInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AddressInput(<redacted>)")
    }
}
pub struct Builder<'a> {
    identities: &'a mut [IdentitySlot],
    addresses: &'a mut [AddressSlot],
    identity_count: usize,
    address_count: usize,
    account: Option<Account>,
    owner: NonZeroU64,
    failure: Option<Error>,
}
impl fmt::Debug for Builder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IdentityBuilder(<redacted>)")
    }
}
impl<'a> Builder<'a> {
    pub fn new(
        arena: &text::Builder<'_>,
        identities: &'a mut [IdentitySlot],
        addresses: &'a mut [AddressSlot],
    ) -> Result<Self, Error> {
        if identities.is_empty()
            || identities.len() > identity::MAX_IDENTITIES
            || addresses.len() > MAX_ADDRESS_CELLS
        {
            return Err(err(Code::Capacity, None));
        }
        Ok(Self {
            identities,
            addresses,
            identity_count: 0,
            address_count: 0,
            account: None,
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
        let result = if arena.owner() != self.owner {
            Err(err(Code::ForeignArena, Some(at)))
        } else {
            action(self, arena)
        };
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    fn intern(&mut self, id: IdentityId, at: Location) -> Result<usize, Error> {
        let active = self
            .identities
            .get(..self.identity_count)
            .ok_or_else(invariant)?;
        if let Some(index) = active.iter().position(|slot| slot.id == id) {
            return Ok(index);
        }
        let index = self.identity_count;
        if index >= self.identities.len() {
            let previous = active
                .iter()
                .find(|slot| !slot.declared)
                .map(|slot| slot.location);
            return Err(Error {
                code: Code::Capacity,
                location: Some(at),
                previous,
            });
        }
        let next = index.checked_add(1).ok_or_else(invariant)?;
        let slot = self
            .identities
            .get_mut(index)
            .ok_or_else(|| err(Code::Capacity, Some(at)))?;
        *slot = IdentitySlot {
            id,
            location: at,
            ..IdentitySlot::EMPTY
        };
        self.identity_count = next;
        Ok(index)
    }
    pub fn account(
        &mut self,
        arena: &mut text::Builder<'_>,
        id: AccountId,
        username: &str,
        name: &str,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            if let Some(old) = this.account {
                return Err(Error {
                    code: Code::DuplicateAccount,
                    location: Some(at),
                    previous: Some(old.location),
                });
            }
            if username.is_empty()
                || username.len() > 254
                || !username
                    .bytes()
                    .all(|b| (33..=126).contains(&b) && b != b':')
            {
                return Err(err(Code::Username, Some(at)));
            }
            check_name(name, at)?;
            let username = store(arena, username, at)?;
            let name = store(arena, name, at)?;
            this.account = Some(Account {
                id,
                username,
                name,
                location: at,
            });
            Ok(())
        })
    }
    pub fn identity(
        &mut self,
        arena: &mut text::Builder<'_>,
        input: IdentityInput<'_>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            check_name(input.name, at)?;
            check_mailbox(input.email, true, at)?;
            for path in [input.text_signature_file, input.html_signature_file]
                .into_iter()
                .flatten()
            {
                values::absolute_path(path).map_err(|_| err(Code::SignaturePath, Some(at)))?;
            }
            let index = this.intern(input.id, at)?;
            let old = *this.identities.get(index).ok_or_else(invariant)?;
            if old.declared {
                return Err(Error {
                    code: Code::DuplicateIdentity,
                    location: Some(at),
                    previous: Some(old.location),
                });
            }
            let name = store(arena, input.name, at)?;
            let email = store(arena, input.email, at)?;
            let text_path = store_optional(arena, input.text_signature_file, at)?;
            let html_path = store_optional(arena, input.html_signature_file, at)?;
            *this.identities.get_mut(index).ok_or_else(invariant)? = IdentitySlot {
                id: input.id,
                account: input.account,
                name,
                email,
                text_path,
                html_path,
                chains: old.chains,
                location: at,
                declared: true,
                reply: input.reply_to,
                bcc: input.bcc,
                has_text: input.text_signature_file.is_some(),
                has_html: input.html_signature_file.is_some(),
            };
            Ok(())
        })
    }
    pub fn address(
        &mut self,
        arena: &mut text::Builder<'_>,
        input: AddressInput<'_>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            if let Some(name) = input.name {
                check_name(name, at)?;
            }
            check_mailbox(input.email, false, at)?;
            let identity = this.intern(input.identity, at)?;
            let chain = *this
                .identities
                .get(identity)
                .and_then(|slot| slot.chains.get(input.list.index()))
                .ok_or_else(invariant)?;
            if usize::from(chain.count) >= identity::MAX_ADDRESSES {
                return Err(err(Code::AddressCount, Some(at)));
            }
            let index = this.address_count;
            if index >= this.addresses.len() {
                return Err(err(Code::Capacity, Some(at)));
            }
            let encoded = u16::try_from(index).map_err(|_| invariant())?;
            let count = chain.count.checked_add(1).ok_or_else(invariant)?;
            let next_count = index.checked_add(1).ok_or_else(invariant)?;
            let name = store_optional(arena, input.name, at)?;
            let email = store(arena, input.email, at)?;
            *this.addresses.get_mut(index).ok_or_else(invariant)? = AddressSlot {
                name,
                email,
                next: NONE,
                location: at,
                has_name: input.name.is_some(),
            };
            if chain.last != NONE {
                this.addresses
                    .get_mut(usize::from(chain.last))
                    .ok_or_else(invariant)?
                    .next = encoded;
            }
            *this
                .identities
                .get_mut(identity)
                .and_then(|slot| slot.chains.get_mut(input.list.index()))
                .ok_or_else(invariant)? = Chain {
                first: if chain.count == 0 {
                    encoded
                } else {
                    chain.first
                },
                last: encoded,
                count,
            };
            this.address_count = next_count;
            Ok(())
        })
    }
    pub fn finish(self) -> Result<Records<'a>, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let account = self
            .account
            .ok_or_else(|| err(Code::MissingAccount, None))?;
        if self.identity_count == 0 {
            return Err(err(Code::MissingIdentity, None));
        }
        let identities = self
            .identities
            .get_mut(..self.identity_count)
            .ok_or_else(invariant)?;
        let addresses = self
            .addresses
            .get(..self.address_count)
            .ok_or_else(invariant)?;
        for slot in identities.iter() {
            if !slot.declared {
                return Err(err(Code::UnknownIdentity, Some(slot.location)));
            }
            if slot.account != account.id {
                return Err(err(Code::UnknownAccount, Some(slot.location)));
            }
            for (chain, enabled) in slot.chains.iter().zip([slot.reply, slot.bcc]) {
                if !enabled && chain.count != 0 {
                    let at = addresses
                        .get(usize::from(chain.first))
                        .ok_or_else(invariant)?
                        .location;
                    return Err(err(Code::DisabledList, Some(at)));
                }
            }
        }
        identities.sort_unstable_by(|a, b| a.id.as_bytes().cmp(b.id.as_bytes()));
        Ok(Records {
            identities,
            addresses,
            account,
            owner: self.owner,
        })
    }
}
fn check_name(input: &str, at: Location) -> Result<(), Error> {
    if input.len() > identity::MAX_NAME_BYTES {
        Err(err(Code::Name, Some(at)))
    } else {
        Ok(())
    }
}
fn check_mailbox(input: &str, sender: bool, at: Location) -> Result<(), Error> {
    let mut scratch = [0; identity::MAX_EMAIL_BYTES];
    let key = values::mailbox_key(input, &mut scratch).map_err(|_| err(Code::Mailbox, Some(at)))?;
    if sender && key.starts_with(b"*\0") {
        return Err(err(Code::Wildcard, Some(at)));
    }
    Ok(())
}
fn store_optional(
    arena: &mut text::Builder<'_>,
    input: Option<&str>,
    at: Location,
) -> Result<text::Span, Error> {
    match input {
        Some(value) => store(arena, value, at),
        None => Ok(text::Span::EMPTY),
    }
}
fn store(arena: &mut text::Builder<'_>, input: &str, at: Location) -> Result<text::Span, Error> {
    let handle = arena
        .append(input.as_bytes())
        .map_err(|_| err(Code::Text, Some(at)))?;
    arena.compact(handle).map_err(|_| invariant())
}
pub struct Records<'a> {
    identities: &'a mut [IdentitySlot],
    addresses: &'a [AddressSlot],
    account: Account,
    owner: NonZeroU64,
}
impl fmt::Debug for Records<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IdentityRecords(<redacted>)")
    }
}
impl<'a> Records<'a> {
    pub fn view_live<'s, 't>(&'s self, text: &'t text::Builder<'_>) -> Result<View<'s, 't>, Error> {
        self.view(text.borrowed_view().map_err(|_| invariant())?)
    }
    pub fn view<'s, 't>(&'s self, text: text::View<'t>) -> Result<View<'s, 't>, Error> {
        if self.owner != text.owner() {
            return Err(err(Code::ForeignArena, None));
        }
        Ok(View {
            identities: self.identities,
            addresses: self.addresses,
            account: self.account,
            owner: self.owner,
            text,
        })
    }
}
#[derive(Clone, Copy)]
pub struct View<'s, 't> {
    identities: &'s [IdentitySlot],
    addresses: &'s [AddressSlot],
    account: Account,
    text: text::View<'t>,
    owner: NonZeroU64,
}
impl fmt::Debug for View<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IdentityView(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct AccountView<'t> {
    pub id: AccountId,
    pub username: &'t str,
    pub name: &'t str,
}
impl fmt::Debug for AccountView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccountView(<redacted>)")
    }
}
pub struct IdentityView<'s, 't> {
    pub id: IdentityId,
    pub name: &'t str,
    pub email: &'t str,
    pub reply_to: Option<AddressIter<'s, 't>>,
    pub bcc: Option<AddressIter<'s, 't>>,
    pub text_signature_file: Option<&'t str>,
    pub html_signature_file: Option<&'t str>,
}
impl fmt::Debug for IdentityView<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IdentityRecord(<redacted>)")
    }
}
impl<'s, 't> View<'s, 't> {
    pub fn len(self) -> usize {
        self.identities.len()
    }
    pub fn is_empty(self) -> bool {
        self.identities.is_empty()
    }
    pub fn account(self) -> Result<AccountView<'t>, Error> {
        Ok(AccountView {
            id: self.account.id,
            username: read(self.text, self.owner, self.account.username)?,
            name: read(self.text, self.owner, self.account.name)?,
        })
    }
    pub fn identity(self, index: usize) -> Result<Option<IdentityView<'s, 't>>, Error> {
        let Some(slot) = self.identities.get(index) else {
            return Ok(None);
        };
        let make_list = |list: List, enabled: bool| -> Result<Option<AddressIter<'s, 't>>, Error> {
            if !enabled {
                return Ok(None);
            }
            let chain = slot.chains.get(list.index()).ok_or_else(invariant)?;
            Ok(Some(AddressIter {
                addresses: self.addresses,
                text: self.text,
                owner: self.owner,
                next: chain.first,
                remaining: chain.count,
                failed: false,
            }))
        };
        Ok(Some(IdentityView {
            id: slot.id,
            name: read(self.text, self.owner, slot.name)?,
            email: read(self.text, self.owner, slot.email)?,
            reply_to: make_list(List::ReplyTo, slot.reply)?,
            bcc: make_list(List::Bcc, slot.bcc)?,
            text_signature_file: if slot.has_text {
                Some(read(self.text, self.owner, slot.text_path)?)
            } else {
                None
            },
            html_signature_file: if slot.has_html {
                Some(read(self.text, self.owner, slot.html_path)?)
            } else {
                None
            },
        }))
    }
}
fn read(text: text::View<'_>, owner: NonZeroU64, span: text::Span) -> Result<&str, Error> {
    std::str::from_utf8(text.read_span(owner, span).map_err(|_| invariant())?)
        .map_err(|_| invariant())
}
pub struct AddressIter<'s, 't> {
    addresses: &'s [AddressSlot],
    text: text::View<'t>,
    next: u16,
    remaining: u8,
    failed: bool,
    owner: NonZeroU64,
}
impl fmt::Debug for AddressIter<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IdentityAddresses(<redacted>)")
    }
}
impl std::iter::FusedIterator for AddressIter<'_, '_> {}
impl<'t> Iterator for AddressIter<'_, 't> {
    type Item = Result<identity::Address<'t>, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        if self.remaining == 0 {
            if self.next == NONE {
                return None;
            }
            self.failed = true;
            return Some(Err(invariant()));
        }
        let result = (|| {
            let slot = self
                .addresses
                .get(usize::from(self.next))
                .ok_or_else(invariant)?;
            self.remaining = self.remaining.checked_sub(1).ok_or_else(invariant)?;
            self.next = slot.next;
            Ok(identity::Address {
                name: if slot.has_name {
                    Some(read(self.text, self.owner, slot.name)?)
                } else {
                    None
                },
                email: read(self.text, self.owner, slot.email)?,
            })
        })();
        if result.is_err() {
            self.failed = true;
        }
        Some(result)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn account(n: u8) -> AccountId {
        AccountId::from_bytes([n; 16])
    }
    fn id(n: u8) -> IdentityId {
        IdentityId::from_bytes([n; 16])
    }
    fn at(n: u32) -> Location {
        Location {
            line: NonZeroU32::new(n).unwrap(),
            column: 2,
        }
    }
    fn input(n: u8) -> IdentityInput<'static> {
        IdentityInput {
            id: id(n),
            account: account(1),
            name: "",
            email: "Sender@Example.TEST",
            reply_to: false,
            bcc: false,
            text_signature_file: None,
            html_signature_file: None,
        }
    }
    fn storage() -> (Vec<u8>, Vec<IdentitySlot>, Vec<AddressSlot>) {
        (
            vec![0; 4096],
            vec![IdentitySlot::EMPTY; 4],
            vec![AddressSlot::EMPTY; 40],
        )
    }
    #[test]
    fn forward_lists_keep_order_and_null_empty_names_after_identity_sort() {
        let _lock = lock();
        let (mut bytes, mut ids, mut addresses) = storage();
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        for (identity, list, name, email) in [
            (id(2), List::ReplyTo, None, "First@A.test"),
            (id(1), List::Bcc, Some("Bee"), "b@a.test"),
            (id(2), List::ReplyTo, Some(""), "First@A.test"),
            (id(2), List::ReplyTo, Some("MiXeD"), "last@a.test"),
        ] {
            b.address(
                &mut arena,
                AddressInput {
                    identity,
                    list,
                    name,
                    email,
                },
                at(1),
            )
            .unwrap();
        }
        b.identity(
            &mut arena,
            IdentityInput {
                reply_to: true,
                ..input(2)
            },
            at(2),
        )
        .unwrap();
        b.identity(
            &mut arena,
            IdentityInput {
                reply_to: true,
                bcc: true,
                ..input(1)
            },
            at(3),
        )
        .unwrap();
        b.account(&mut arena, account(1), "CaseSensitive", "Owner", at(4))
            .unwrap();
        let records = b.finish().unwrap();
        let text = arena.freeze().unwrap();
        let view = records.view(text).unwrap();
        assert_eq!(view.len(), 2);
        assert!(!view.is_empty());
        assert!(view.identity(2).unwrap().is_none());
        let acc = view.account().unwrap();
        assert_eq!(acc.id, account(1));
        assert_eq!(acc.username, "CaseSensitive");
        assert_eq!(acc.name, "Owner");
        let first = view.identity(0).unwrap().unwrap();
        assert_eq!(first.id, id(1));
        assert_eq!(first.email, "Sender@Example.TEST");
        assert_eq!(first.name, "");
        assert!(first.reply_to.unwrap().next().is_none());
        let mut bcc = first.bcc.unwrap();
        assert_eq!(bcc.next().unwrap().unwrap().name, Some("Bee"));
        assert!(bcc.next().is_none());
        let second = view.identity(1).unwrap().unwrap();
        assert_eq!(second.id, id(2));
        assert!(second.bcc.is_none());
        let mut list = second.reply_to.unwrap();
        for (name, email) in [
            (None, "First@A.test"),
            (Some(""), "First@A.test"),
            (Some("MiXeD"), "last@a.test"),
        ] {
            let actual = list.next().unwrap().unwrap();
            assert_eq!(actual.name, name);
            assert_eq!(actual.email, email);
        }
        assert!(list.next().is_none());
        assert!(list.next().is_none());
    }
    #[test]
    fn signature_paths_stay_unresolved_and_defaults_are_absent() {
        let _lock = lock();
        let (mut bytes, mut ids, mut addresses) = storage();
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        b.account(&mut arena, account(1), "owner", "", at(1))
            .unwrap();
        b.identity(&mut arena, input(1), at(2)).unwrap();
        b.identity(
            &mut arena,
            IdentityInput {
                text_signature_file: Some("/Missing/MiXeD.txt"),
                html_signature_file: Some("/signatures/html"),
                ..input(2)
            },
            at(3),
        )
        .unwrap();
        let records = b.finish().unwrap();
        let view = records.view(arena.freeze().unwrap()).unwrap();
        let first = view.identity(0).unwrap().unwrap();
        assert_eq!(first.text_signature_file, None);
        assert_eq!(first.html_signature_file, None);
        let second = view.identity(1).unwrap().unwrap();
        assert_eq!(second.text_signature_file, Some("/Missing/MiXeD.txt"));
        assert_eq!(second.html_signature_file, Some("/signatures/html"));
    }
    #[test]
    fn invalid_scalar_inputs_poison_candidate_and_sender_wildcards_are_rejected() {
        let _lock = lock();
        for (email, expected) in [
            ("*@a.test", Code::Wildcard),
            ("\"*\"@a.test", Code::Wildcard),
            ("\"\\*\"@a.test", Code::Wildcard),
            ("bad", Code::Mailbox),
            ("Postmaster", Code::Mailbox),
        ] {
            let (mut bytes, mut ids, mut addresses) = storage();
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
            let e = b
                .identity(&mut arena, IdentityInput { email, ..input(1) }, at(9))
                .unwrap_err();
            assert_eq!(e.code, expected);
            assert_eq!(e.location, Some(at(9)));
            assert_eq!(
                b.account(&mut arena, account(1), "owner", "", at(10)),
                Err(e)
            );
            assert_eq!(b.finish().unwrap_err(), e);
        }
        for username in ["", "contains space", "colon:here", "é", "newline\n"] {
            let (mut bytes, mut ids, mut addresses) = storage();
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
            assert_eq!(
                b.account(&mut arena, account(1), username, "", at(1))
                    .unwrap_err()
                    .code,
                Code::Username
            );
        }
        for path in ["relative", "/a/../b", "/a//b"] {
            let (mut bytes, mut ids, mut addresses) = storage();
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
            assert_eq!(
                b.identity(
                    &mut arena,
                    IdentityInput {
                        text_signature_file: Some(path),
                        ..input(1)
                    },
                    at(1)
                )
                .unwrap_err()
                .code,
                Code::SignaturePath
            );
        }
    }
    #[test]
    fn missing_duplicate_and_unknown_references_refuse_finalization() {
        let _lock = lock();
        for scenario in 0..7 {
            let (mut bytes, mut ids, mut addresses) = storage();
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
            if scenario != 0 {
                b.account(&mut arena, account(1), "owner", "", at(1))
                    .unwrap();
            }
            let expected = match scenario {
                0 => Code::MissingAccount,
                1 => Code::MissingIdentity,
                2 => {
                    b.address(
                        &mut arena,
                        AddressInput {
                            identity: id(2),
                            list: List::ReplyTo,
                            name: None,
                            email: "a@b.test",
                        },
                        at(2),
                    )
                    .unwrap();
                    Code::UnknownIdentity
                }
                3 => {
                    b.identity(
                        &mut arena,
                        IdentityInput {
                            account: account(2),
                            ..input(1)
                        },
                        at(3),
                    )
                    .unwrap();
                    Code::UnknownAccount
                }
                4 => {
                    b.identity(&mut arena, input(1), at(2)).unwrap();
                    b.address(
                        &mut arena,
                        AddressInput {
                            identity: id(1),
                            list: List::ReplyTo,
                            name: None,
                            email: "a@b.test",
                        },
                        at(3),
                    )
                    .unwrap();
                    Code::DisabledList
                }
                5 => {
                    let e = b
                        .account(&mut arena, account(1), "owner", "", at(2))
                        .unwrap_err();
                    assert_eq!(e.previous, Some(at(1)));
                    assert_eq!(e.to_string(), "config_identity_duplicate_account at line 2, byte column 2 (previously declared at line 1, byte column 2)");
                    Code::DuplicateAccount
                }
                _ => {
                    b.identity(&mut arena, input(1), at(2)).unwrap();
                    let e = b.identity(&mut arena, input(1), at(3)).unwrap_err();
                    assert_eq!(e.previous, Some(at(2)));
                    assert_eq!(e.to_string(), "config_identity_duplicate at line 3, byte column 2 (previously declared at line 2, byte column 2)");
                    Code::DuplicateIdentity
                }
            };
            assert_eq!(
                b.finish().unwrap_err().code,
                expected,
                "scenario {scenario}"
            );
        }
    }
    #[test]
    fn foreign_arenas_cannot_mutate_or_read_compact_references() {
        let _lock = lock();
        let (mut bytes, mut ids, mut addresses) = storage();
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut other_bytes = [0; 4096];
        let mut other = text::Builder::new(&mut other_bytes).unwrap();
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        let e = b
            .account(&mut other, account(1), "owner", "", at(1))
            .unwrap_err();
        assert_eq!(e.code, Code::ForeignArena);
        assert_eq!(b.finish().unwrap_err(), e);
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        b.account(&mut arena, account(1), "owner", "", at(1))
            .unwrap();
        b.identity(&mut arena, input(1), at(2)).unwrap();
        let records = b.finish().unwrap();
        assert_eq!(
            records.view(other.freeze().unwrap()).unwrap_err().code,
            Code::ForeignArena
        );
        assert_eq!(records.view(arena.freeze().unwrap()).unwrap().len(), 1);
    }
    #[test]
    fn full_counts_and_actual_layouts_fit_the_metadata_reservations() {
        let _lock = lock();
        assert!(std::mem::size_of::<IdentitySlot>() <= 128);
        assert!(std::mem::size_of::<AddressSlot>() <= 32);
        assert!(std::mem::size_of::<Builder<'_>>() <= 256);
        let mut bytes = vec![0; text::MAX_BYTES];
        let mut ids = vec![IdentitySlot::EMPTY; identity::MAX_IDENTITIES];
        let mut addresses = vec![AddressSlot::EMPTY; MAX_ADDRESS_CELLS];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        b.account(&mut arena, account(1), "owner", "", at(1))
            .unwrap();
        for n in 0..64 {
            b.identity(
                &mut arena,
                IdentityInput {
                    reply_to: true,
                    bcc: true,
                    ..input(n)
                },
                at(2),
            )
            .unwrap();
            for list in [List::ReplyTo, List::Bcc] {
                for _ in 0..16 {
                    b.address(
                        &mut arena,
                        AddressInput {
                            identity: id(n),
                            list,
                            name: None,
                            email: "*@a.test",
                        },
                        at(3),
                    )
                    .unwrap();
                }
            }
        }
        assert_eq!(b.address_count, MAX_ADDRESS_CELLS);
        let records = b.finish().unwrap();
        let view = records.view(arena.freeze().unwrap()).unwrap();
        assert_eq!(view.len(), 64);
        for n in 0..64 {
            let item = view.identity(n).unwrap().unwrap();
            for list in [item.reply_to, item.bcc] {
                let count = list.unwrap().inspect(|x| assert!(x.is_ok())).count();
                assert_eq!(count, 16);
            }
        }
    }
    #[test]
    fn list_and_table_saturation_poison_without_overflow() {
        let _lock = lock();
        for scenario in 0..3 {
            let (mut bytes, mut ids, mut addresses) = storage();
            if scenario == 1 {
                addresses.clear();
            }
            if scenario == 2 {
                ids.truncate(1);
            }
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
            let error = if scenario == 2 {
                b.identity(&mut arena, input(1), at(1)).unwrap();
                b.identity(&mut arena, input(2), at(2)).unwrap_err()
            } else {
                if scenario == 0 {
                    for _ in 0..16 {
                        b.address(
                            &mut arena,
                            AddressInput {
                                identity: id(1),
                                list: List::Bcc,
                                name: None,
                                email: "a@b.test",
                            },
                            at(1),
                        )
                        .unwrap();
                    }
                }
                b.address(
                    &mut arena,
                    AddressInput {
                        identity: id(1),
                        list: List::Bcc,
                        name: None,
                        email: "a@b.test",
                    },
                    at(2),
                )
                .unwrap_err()
            };
            assert_eq!(
                error.code,
                if scenario == 0 {
                    Code::AddressCount
                } else {
                    Code::Capacity
                }
            );
            assert_eq!(b.finish().unwrap_err(), error);
        }
    }
    #[test]
    fn partial_text_exhaustion_and_oversized_fields_cannot_publish() {
        let _lock = lock();
        let (mut bytes, mut ids, mut addresses) = storage();
        bytes.truncate(6);
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        let error = b
            .account(&mut arena, account(1), "owner", "too-long", at(1))
            .unwrap_err();
        assert_eq!(error.code, Code::Text);
        assert_eq!(arena.used(), 5);
        assert_eq!(b.finish().unwrap_err(), error);
        for scenario in 0..3 {
            let (mut bytes, mut ids, mut addresses) = storage();
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
            let name = "x".repeat(identity::MAX_NAME_BYTES + 1);
            let username = "x".repeat(255);
            let error = match scenario {
                0 => b.account(&mut arena, account(1), &username, "", at(1)),
                1 => b.identity(
                    &mut arena,
                    IdentityInput {
                        name: &name,
                        ..input(1)
                    },
                    at(1),
                ),
                _ => b.address(
                    &mut arena,
                    AddressInput {
                        identity: id(1),
                        list: List::Bcc,
                        name: Some(&name),
                        email: "a@b.test",
                    },
                    at(1),
                ),
            }
            .unwrap_err();
            assert_eq!(
                error.code,
                if scenario == 0 {
                    Code::Username
                } else {
                    Code::Name
                }
            );
            assert_eq!(arena.used(), 0);
        }
    }
    #[test]
    fn sender_case_and_quoted_spelling_remain_visible() {
        let _lock = lock();
        let (mut bytes, mut ids, mut addresses) = storage();
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        b.account(&mut arena, account(1), "owner", "", at(1))
            .unwrap();
        b.identity(
            &mut arena,
            IdentityInput {
                email: "PoStMaStEr@Example.TEST",
                ..input(1)
            },
            at(2),
        )
        .unwrap();
        b.identity(
            &mut arena,
            IdentityInput {
                email: "\"A B\"@Example.TEST",
                ..input(2)
            },
            at(3),
        )
        .unwrap();
        let records = b.finish().unwrap();
        let view = records.view(arena.freeze().unwrap()).unwrap();
        assert_eq!(
            view.identity(0).unwrap().unwrap().email,
            "PoStMaStEr@Example.TEST"
        );
        assert_eq!(
            view.identity(1).unwrap().unwrap().email,
            "\"A B\"@Example.TEST"
        );
        assert_eq!(
            format!("{records:?} {view:?}"),
            "IdentityRecords(<redacted>) IdentityView(<redacted>)"
        );
        let error = err(Code::Mailbox, Some(at(9)));
        assert_eq!(
            error.to_string(),
            "config_identity_mailbox at line 9, byte column 2"
        );
        assert!(std::error::Error::source(&error).is_none());
    }
    #[test]
    fn live_signature_path_reads_leave_capacity_available_for_later_loading() {
        let _lock = lock();
        let (mut bytes, mut ids, mut addresses) = storage();
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        b.account(&mut arena, account(1), "owner", "", at(1))
            .unwrap();
        b.identity(
            &mut arena,
            IdentityInput {
                text_signature_file: Some("/signature"),
                ..input(1)
            },
            at(2),
        )
        .unwrap();
        let records = b.finish().unwrap();
        assert_eq!(
            records
                .view_live(&arena)
                .unwrap()
                .identity(0)
                .unwrap()
                .unwrap()
                .text_signature_file,
            Some("/signature")
        );
        let content = arena.append(b"Later signature bytes").unwrap();
        assert_eq!(
            records
                .view_live(&arena)
                .unwrap()
                .identity(0)
                .unwrap()
                .unwrap()
                .text_signature_file,
            Some("/signature")
        );
        let mut other_bytes = [0; 8];
        let other = text::Builder::new(&mut other_bytes).unwrap();
        assert_eq!(
            records.view_live(&other).unwrap_err().code,
            Code::ForeignArena
        );
        let frozen = arena.freeze().unwrap();
        assert_eq!(frozen.text(content), Ok("Later signature bytes"));
        assert_eq!(records.view(frozen).unwrap().len(), 1);
    }
    #[test]
    fn exact_username_and_name_limits_are_accepted() {
        let _lock = lock();
        let (mut bytes, mut ids, mut addresses) = storage();
        bytes.resize(20 * 1024, 0);
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        let username = "U".repeat(254);
        let name = "N".repeat(identity::MAX_NAME_BYTES);
        b.account(&mut arena, account(1), &username, &name, at(1))
            .unwrap();
        b.identity(
            &mut arena,
            IdentityInput {
                name: &name,
                reply_to: true,
                ..input(1)
            },
            at(2),
        )
        .unwrap();
        b.address(
            &mut arena,
            AddressInput {
                identity: id(1),
                list: List::ReplyTo,
                name: Some(&name),
                email: "a@b.test",
            },
            at(3),
        )
        .unwrap();
        let records = b.finish().unwrap();
        let view = records.view(arena.freeze().unwrap()).unwrap();
        assert_eq!(view.account().unwrap().username, username);
        assert_eq!(view.account().unwrap().name, name);
        let item = view.identity(0).unwrap().unwrap();
        assert_eq!(item.name, name);
        assert_eq!(
            item.reply_to.unwrap().next().unwrap().unwrap().name,
            Some(name.as_str())
        );
    }
    #[test]
    fn saturated_forward_targets_report_the_unresolved_source() {
        let _lock = lock();
        let (mut bytes, mut ids, mut addresses) = storage();
        ids.truncate(1);
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena, &mut ids, &mut addresses).unwrap();
        b.address(
            &mut arena,
            AddressInput {
                identity: id(9),
                list: List::ReplyTo,
                name: None,
                email: "a@b.test",
            },
            at(5),
        )
        .unwrap();
        let e = b.identity(&mut arena, input(1), at(20)).unwrap_err();
        assert_eq!(e.code, Code::Capacity);
        assert_eq!(e.location, Some(at(20)));
        assert_eq!(e.previous, Some(at(5)));
        assert_eq!(e.to_string(),"config_identity_capacity at line 20, byte column 2 (earlier unresolved reference at line 5, byte column 2)");
        assert_eq!(b.finish().unwrap_err(), e);
    }
    #[test]
    fn corrupt_address_links_fail_once_then_remain_fused() {
        fn requires_fused<T: std::iter::FusedIterator>(_: &T) {}
        let _lock = lock();
        let mut bytes = [];
        let arena = text::Builder::new(&mut bytes).unwrap();
        let text = arena.freeze().unwrap();
        for (next, remaining, tail, successes) in [
            (NONE, 1, NONE, 0),
            (0, 0, NONE, 0),
            (0, 2, NONE, 1),
            (0, 1, 0, 1),
        ] {
            let rows = [AddressSlot {
                next: tail,
                ..AddressSlot::EMPTY
            }];
            let mut iter = AddressIter {
                addresses: &rows,
                text,
                owner: text.owner(),
                next,
                remaining,
                failed: false,
            };
            requires_fused(&iter);
            for _ in 0..successes {
                assert!(iter.next().unwrap().is_ok());
            }
            assert_eq!(iter.next().unwrap().unwrap_err().code, Code::Invariant);
            assert!(iter.next().is_none());
            assert!(iter.next().is_none());
        }
    }
}
