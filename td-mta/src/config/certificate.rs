//! Structural certificate/ACME configuration; no material or provider authority.
use super::{endpoint::HttpsUri, syntax::Location, text, values};
use std::{fmt, num::NonZeroU64};
pub const MAX_PROFILES: usize = 16;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    ForeignArena,
    Profile,
    ChainPath,
    KeyPath,
    CaPath,
    Directory,
    Contact,
    Terms,
    DuplicateProfile,
    DuplicateAcme,
    MissingProfiles,
    MissingAcme,
    UnusedAcme,
    Text,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_certificate_capacity",
            Self::ForeignArena => "config_certificate_foreign_arena",
            Self::Profile => "config_certificate_profile",
            Self::ChainPath => "config_certificate_chain_path",
            Self::KeyPath => "config_certificate_key_path",
            Self::CaPath => "config_certificate_ca_path",
            Self::Directory => "config_certificate_directory",
            Self::Contact => "config_certificate_contact",
            Self::Terms => "config_certificate_terms",
            Self::DuplicateProfile => "config_certificate_duplicate_profile",
            Self::DuplicateAcme => "config_certificate_duplicate_acme",
            Self::MissingProfiles => "config_certificate_missing_profiles",
            Self::MissingAcme => "config_certificate_missing_acme",
            Self::UnusedAcme => "config_certificate_unused_acme",
            Self::Text => "config_certificate_text",
            Self::Invariant => "config_certificate_invariant",
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
                " (previously declared at line {}, byte column {})",
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Acme,
    Files,
}
impl Mode {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Acme => "acme",
            Self::Files => "files",
        }
    }
}
#[derive(Clone, Copy)]
pub enum ProfileInput<'a> {
    Acme,
    Files {
        chain_file: &'a str,
        key_file: &'a str,
    },
}
impl fmt::Debug for ProfileInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CertificateInput(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct AcmeInput<'a> {
    pub directory: &'a str,
    pub contact: &'a str,
    pub terms_accepted: bool,
    pub ca_file: Option<&'a str>,
}
impl fmt::Debug for AcmeInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AcmeInput(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct Slot {
    name: text::Span,
    chain: text::Span,
    key: text::Span,
    mode: Mode,
    location: Option<Location>,
}
impl Slot {
    pub const EMPTY: Self = Self {
        name: text::Span::EMPTY,
        chain: text::Span::EMPTY,
        key: text::Span::EMPTY,
        mode: Mode::Acme,
        location: None,
    };
}
#[derive(Clone, Copy)]
struct AcmeSlot {
    directory: text::Span,
    contact: text::Span,
    ca: text::Span,
    has_ca: bool,
    location: Location,
}
const _: [(); 1] = [(); (std::mem::size_of::<Slot>() <= 128) as usize];
const _: [(); 1] = [(); (std::mem::size_of::<AcmeSlot>() <= 128) as usize];
pub struct Builder<'a> {
    slots: &'a mut [Slot],
    count: usize,
    acme: Option<AcmeSlot>,
    owner: NonZeroU64,
    failure: Option<Error>,
}
const _: [(); 1] = [(); (std::mem::size_of::<Builder<'_>>() <= 256) as usize];
impl fmt::Debug for Builder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CertificateBuilder(<redacted>)")
    }
}
impl<'a> Builder<'a> {
    pub fn new(arena: &text::Builder<'_>, slots: &'a mut [Slot]) -> Result<Self, Error> {
        if slots.is_empty() || slots.len() > MAX_PROFILES {
            return Err(err(Code::Capacity, None));
        }
        Ok(Self {
            slots,
            count: 0,
            acme: None,
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
    pub fn profile(
        &mut self,
        arena: &mut text::Builder<'_>,
        name: &str,
        input: ProfileInput<'_>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            values::profile_name(name).map_err(|_| err(Code::Profile, Some(at)))?;
            let view = arena.borrowed_view().map_err(|_| invariant())?;
            for slot in this.slots.get(..this.count).ok_or_else(invariant)? {
                if read(view, this.owner, slot.name)? == name {
                    return Err(duplicate(
                        Code::DuplicateProfile,
                        at,
                        slot.location.ok_or_else(invariant)?,
                    ));
                }
            }
            if let ProfileInput::Files {
                chain_file,
                key_file,
            } = input
            {
                values::absolute_path(chain_file).map_err(|_| err(Code::ChainPath, Some(at)))?;
                values::absolute_path(key_file).map_err(|_| err(Code::KeyPath, Some(at)))?;
            }
            if this.count >= this.slots.len() {
                return Err(err(Code::Capacity, Some(at)));
            }
            let name = store(arena, name, at)?;
            let (mode, chain, key) = match input {
                ProfileInput::Acme => (Mode::Acme, text::Span::EMPTY, text::Span::EMPTY),
                ProfileInput::Files {
                    chain_file,
                    key_file,
                } => (
                    Mode::Files,
                    store(arena, chain_file, at)?,
                    store(arena, key_file, at)?,
                ),
            };
            *this.slots.get_mut(this.count).ok_or_else(invariant)? = Slot {
                name,
                chain,
                key,
                mode,
                location: Some(at),
            };
            this.count = this.count.checked_add(1).ok_or_else(invariant)?;
            Ok(())
        })
    }
    pub fn acme(
        &mut self,
        arena: &mut text::Builder<'_>,
        input: AcmeInput<'_>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            if let Some(old) = this.acme {
                return Err(duplicate(Code::DuplicateAcme, at, old.location));
            }
            if !input.terms_accepted {
                return Err(err(Code::Terms, Some(at)));
            }
            HttpsUri::parse(input.directory).map_err(|_| err(Code::Directory, Some(at)))?;
            // Validate syntax while retaining the operator's contact spelling.
            let mut mailbox = [0; crate::format::row::MAX_ADDRESS];
            values::mailbox_key(input.contact, &mut mailbox)
                .map_err(|_| err(Code::Contact, Some(at)))?;
            if let Some(ca) = input.ca_file {
                values::absolute_path(ca).map_err(|_| err(Code::CaPath, Some(at)))?;
            }
            let directory = store(arena, input.directory, at)?;
            let contact = store(arena, input.contact, at)?;
            let ca = match input.ca_file {
                Some(ca) => store(arena, ca, at)?,
                None => text::Span::EMPTY,
            };
            this.acme = Some(AcmeSlot {
                directory,
                contact,
                ca,
                has_ca: input.ca_file.is_some(),
                location: at,
            });
            Ok(())
        })
    }
    pub fn finish(self) -> Result<Records<'a>, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if self.count == 0 {
            return Err(err(Code::MissingProfiles, None));
        }
        let slots = self.slots.get(..self.count).ok_or_else(invariant)?;
        let first_acme = slots.iter().find(|slot| slot.mode == Mode::Acme);
        match (first_acme, self.acme) {
            (Some(slot), None) => return Err(err(Code::MissingAcme, slot.location)),
            (None, Some(acme)) => return Err(err(Code::UnusedAcme, Some(acme.location))),
            _ => {}
        }
        Ok(Records {
            slots,
            acme: self.acme,
            owner: self.owner,
        })
    }
}
fn store(arena: &mut text::Builder<'_>, input: &str, at: Location) -> Result<text::Span, Error> {
    let h = arena
        .append(input.as_bytes())
        .map_err(|_| err(Code::Text, Some(at)))?;
    arena.compact(h).map_err(|_| invariant())
}
fn read(text: text::View<'_>, owner: NonZeroU64, span: text::Span) -> Result<&str, Error> {
    std::str::from_utf8(text.read_span(owner, span).map_err(|_| invariant())?)
        .map_err(|_| invariant())
}
pub struct Records<'a> {
    slots: &'a [Slot],
    acme: Option<AcmeSlot>,
    owner: NonZeroU64,
}
const _: [(); 1] = [(); (std::mem::size_of::<Records<'_>>() <= 128) as usize];
impl fmt::Debug for Records<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CertificateRecords(<redacted>)")
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
            acme: self.acme,
            owner: self.owner,
            text,
        })
    }
}
#[derive(Clone, Copy)]
pub struct View<'s, 't> {
    slots: &'s [Slot],
    acme: Option<AcmeSlot>,
    owner: NonZeroU64,
    text: text::View<'t>,
}
impl fmt::Debug for View<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CertificateView(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct Profile<'t> {
    pub name: &'t str,
    pub mode: Mode,
    pub chain_file: Option<&'t str>,
    pub key_file: Option<&'t str>,
    pub location: Location,
}
impl fmt::Debug for Profile<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CertificateProfile(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct Acme<'t> {
    pub directory: HttpsUri<'t>,
    pub contact: &'t str,
    pub ca_file: Option<&'t str>,
    pub location: Location,
}
impl fmt::Debug for Acme<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Acme(<redacted>)")
    }
}
impl<'t> View<'_, 't> {
    pub fn len(self) -> usize {
        self.slots.len()
    }
    pub fn is_empty(self) -> bool {
        self.slots.is_empty()
    }
    pub fn profile(self, index: usize) -> Result<Option<Profile<'t>>, Error> {
        let Some(s) = self.slots.get(index) else {
            return Ok(None);
        };
        let (chain_file, key_file) = if s.mode == Mode::Files {
            (
                Some(read(self.text, self.owner, s.chain)?),
                Some(read(self.text, self.owner, s.key)?),
            )
        } else {
            (None, None)
        };
        Ok(Some(Profile {
            name: read(self.text, self.owner, s.name)?,
            mode: s.mode,
            chain_file,
            key_file,
            location: s.location.ok_or_else(invariant)?,
        }))
    }
    pub fn acme(self) -> Result<Option<Acme<'t>>, Error> {
        let Some(s) = self.acme else {
            return Ok(None);
        };
        Ok(Some(Acme {
            directory: HttpsUri::parse(read(self.text, self.owner, s.directory)?)
                .map_err(|_| invariant())?,
            contact: read(self.text, self.owner, s.contact)?,
            ca_file: if s.has_ca {
                Some(read(self.text, self.owner, s.ca)?)
            } else {
                None
            },
            location: s.location,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
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
    fn files() -> ProfileInput<'static> {
        ProfileInput::Files {
            chain_file: "/missing/Chain",
            key_file: "/missing/Key",
        }
    }
    fn acme() -> AcmeInput<'static> {
        AcmeInput {
            directory: "https://CA.Example.test:443/acme/../directory?x=%2f",
            contact: "\"a@b\"@Example.test",
            terms_accepted: true,
            ca_file: Some("/missing/Trust"),
        }
    }
    struct Storage {
        bytes: Vec<u8>,
        slots: Vec<Slot>,
    }
    impl Storage {
        fn new() -> Self {
            Self {
                bytes: vec![0; text::MAX_BYTES],
                slots: vec![Slot::EMPTY; MAX_PROFILES],
            }
        }
    }
    #[test]
    fn mixed_profiles_preserve_paths_and_acme_spelling_through_live_and_frozen_views() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        b.acme(&mut arena, acme(), at(1)).unwrap();
        b.profile(&mut arena, "manual", files(), at(2)).unwrap();
        b.profile(&mut arena, "managed", ProfileInput::Acme, at(3))
            .unwrap();
        let records = b.finish().unwrap();
        {
            let view = records.view_live(&arena).unwrap();
            assert_eq!(view.len(), 2);
            assert!(!view.is_empty());
            assert!(view.profile(2).unwrap().is_none());
            assert!(view.profile(usize::MAX).unwrap().is_none());
            let manual = view.profile(0).unwrap().unwrap();
            assert_eq!(manual.name, "manual");
            assert_eq!(manual.mode, Mode::Files);
            assert_eq!(manual.mode.name(), "files");
            assert_eq!(manual.chain_file, Some("/missing/Chain"));
            assert_eq!(manual.key_file, Some("/missing/Key"));
            assert_eq!(manual.location, at(2));
            let managed = view.profile(1).unwrap().unwrap();
            assert_eq!(managed.name, "managed");
            assert_eq!(managed.mode, Mode::Acme);
            assert_eq!(managed.mode.name(), "acme");
            assert!(managed.chain_file.is_none());
            assert!(managed.key_file.is_none());
            let a = view.acme().unwrap().unwrap();
            assert_eq!(a.directory.raw(), acme().directory);
            assert_eq!(a.contact, acme().contact);
            assert_eq!(a.ca_file, Some("/missing/Trust"));
            assert_eq!(a.location, at(1));
            assert!(a
                .directory
                .same_origin(HttpsUri::parse("https://ca.example.test/order").unwrap()));
        }
        arena.append(b"later protected bytes").unwrap();
        let frozen = records.view(arena.freeze().unwrap()).unwrap();
        assert_eq!(
            frozen.profile(0).unwrap().unwrap().key_file,
            Some("/missing/Key")
        );
        let a = frozen.acme().unwrap().unwrap();
        assert_eq!(a.directory.raw(), acme().directory);
        assert_eq!(a.contact, acme().contact);
        assert_eq!(a.ca_file, acme().ca_file);
    }
    #[test]
    fn required_profiles_acme_presence_and_terms_are_structural_requirements() {
        let _lock = lock();
        for mode in 0..5 {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let expected = match mode {
                0 => Code::MissingProfiles,
                1 => {
                    b.profile(&mut arena, "managed", ProfileInput::Acme, at(4))
                        .unwrap();
                    Code::MissingAcme
                }
                2 => {
                    b.profile(&mut arena, "manual", files(), at(4)).unwrap();
                    b.acme(&mut arena, acme(), at(6)).unwrap();
                    Code::UnusedAcme
                }
                3 => {
                    let e = b
                        .acme(
                            &mut arena,
                            AcmeInput {
                                terms_accepted: false,
                                ..acme()
                            },
                            at(6),
                        )
                        .unwrap_err();
                    assert_eq!(e.code, Code::Terms);
                    assert_eq!(arena.used(), 0);
                    Code::Terms
                }
                _ => {
                    b.profile(&mut arena, "manual", files(), at(4)).unwrap();
                    let records = b.finish().unwrap();
                    assert!(records.view_live(&arena).unwrap().acme().unwrap().is_none());
                    continue;
                }
            };
            let e = b.finish().unwrap_err();
            assert_eq!(e.code, expected);
            if mode == 1 {
                assert_eq!(e.location, Some(at(4)));
            }
            if mode == 2 {
                assert_eq!(e.location, Some(at(6)));
            }
        }
    }
    #[test]
    fn duplicate_coordinates_and_errors_poison_all_mutations() {
        let _lock = lock();
        for is_acme in [false, true] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let (e, code) = if is_acme {
                b.acme(&mut arena, acme(), at(1)).unwrap();
                (
                    b.acme(&mut arena, acme(), at(3)).unwrap_err(),
                    Code::DuplicateAcme,
                )
            } else {
                b.profile(&mut arena, "first", files(), at(1)).unwrap();
                b.profile(&mut arena, "second", files(), at(2)).unwrap();
                (
                    b.profile(&mut arena, "first", ProfileInput::Acme, at(3))
                        .unwrap_err(),
                    Code::DuplicateProfile,
                )
            };
            assert_eq!(
                (e.code, e.location, e.previous),
                (code, Some(at(3)), Some(at(1)))
            );
            assert!(e.to_string().contains("previously declared at line 1"));
            assert_eq!(
                b.profile(&mut arena, "valid", files(), at(4)).unwrap_err(),
                e
            );
            assert_eq!(b.acme(&mut arena, acme(), at(5)).unwrap_err(), e);
            assert_eq!(b.finish().unwrap_err(), e);
        }
    }
    #[test]
    fn first_acme_location_and_full_table_diagnostics_have_fixed_precedence() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            b.profile(&mut arena, "manual", files(), at(1)).unwrap();
            b.profile(&mut arena, "managed_first", ProfileInput::Acme, at(2))
                .unwrap();
            b.profile(&mut arena, "managed_last", ProfileInput::Acme, at(3))
                .unwrap();
            assert_eq!(b.finish().unwrap_err(), err(Code::MissingAcme, Some(at(2))));
        }
        {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            b.acme(&mut arena, acme(), at(4)).unwrap();
            assert_eq!(b.finish().unwrap_err(), err(Code::MissingProfiles, None));
        }
        for duplicate in [false, true] {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            for i in 0..16 {
                b.profile(&mut arena, &format!("p{i}"), files(), at(i + 1))
                    .unwrap();
            }
            let input = ProfileInput::Files {
                chain_file: "relative",
                key_file: "/key",
            };
            let name = if duplicate { "p15" } else { "extra" };
            let e = b.profile(&mut arena, name, input, at(20)).unwrap_err();
            if duplicate {
                assert_eq!((e.code, e.previous), (Code::DuplicateProfile, Some(at(16))));
            } else {
                assert_eq!(e.code, Code::ChainPath);
            }
            assert_eq!(b.finish().unwrap_err(), e);
        }
        assert!(std::mem::size_of::<Records<'_>>() <= 128);
    }
    #[test]
    fn full_capacity_and_reused_cells_only_expose_current_profiles() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            for i in 0..16 {
                b.profile(&mut arena, &format!("p{i}"), files(), at(1))
                    .unwrap();
            }
            let records = b.finish().unwrap();
            let view = records.view_live(&arena).unwrap();
            assert_eq!(view.len(), 16);
            for i in 0..16 {
                assert_eq!(view.profile(i).unwrap().unwrap().name, format!("p{i}"));
            }
        }
        {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            b.profile(&mut arena, "new", ProfileInput::Acme, at(2))
                .unwrap();
            b.acme(
                &mut arena,
                AcmeInput {
                    ca_file: None,
                    ..acme()
                },
                at(3),
            )
            .unwrap();
            let records = b.finish().unwrap();
            let view = records.view_live(&arena).unwrap();
            assert_eq!(view.len(), 1);
            let p = view.profile(0).unwrap().unwrap();
            assert_eq!(p.name, "new");
            assert!(p.chain_file.is_none());
            assert!(p.key_file.is_none());
            assert!(view.acme().unwrap().unwrap().ca_file.is_none());
        }
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        for i in 0..16 {
            b.profile(&mut arena, &format!("p{i}"), files(), at(1))
                .unwrap();
        }
        assert_eq!(
            b.profile(&mut arena, "overflow", files(), at(2))
                .unwrap_err()
                .code,
            Code::Capacity
        );
        assert_eq!(b.finish().unwrap_err().code, Code::Capacity);
        assert!(std::mem::size_of::<Slot>() <= 128);
        assert!(std::mem::size_of::<AcmeSlot>() <= 128);
        assert!(std::mem::size_of::<Builder<'_>>() <= 256);
    }
    #[test]
    fn exact_profile_uri_mailbox_and_path_bounds_fit_without_extra_storage() {
        let _lock = lock();
        let name = format!("p{}", "n".repeat(63));
        let path = format!("/{}", "p".repeat(4094));
        let directory = format!("https://ca.example.test/{}", "a".repeat(4096 - 24));
        assert_eq!(directory.len(), 4096);
        let contact = format!(
            "{}@{}.{}.{}",
            "l".repeat(64),
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(61)
        );
        assert_eq!(contact.len(), 254);
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        b.profile(
            &mut arena,
            &name,
            ProfileInput::Files {
                chain_file: &path,
                key_file: &path,
            },
            at(1),
        )
        .unwrap();
        b.profile(&mut arena, "managed", ProfileInput::Acme, at(2))
            .unwrap();
        b.acme(
            &mut arena,
            AcmeInput {
                directory: &directory,
                contact: &contact,
                terms_accepted: true,
                ca_file: Some(&path),
            },
            at(3),
        )
        .unwrap();
        let records = b.finish().unwrap();
        let view = records.view_live(&arena).unwrap();
        assert_eq!(view.profile(0).unwrap().unwrap().name, name);
        assert_eq!(
            view.profile(0).unwrap().unwrap().key_file,
            Some(path.as_str())
        );
        let a = view.acme().unwrap().unwrap();
        assert_eq!(a.directory.raw(), directory);
        assert_eq!(a.contact, contact);
        assert_eq!(a.ca_file, Some(path.as_str()));
    }
    #[test]
    fn invalid_fields_are_distinguished_before_any_text_is_written() {
        let _lock = lock();
        for (field, value, code) in [
            ("profile", "A".into(), Code::Profile),
            ("profile", "p".repeat(65), Code::Profile),
            ("chain", "relative".into(), Code::ChainPath),
            ("key", "/a/../b".into(), Code::KeyPath),
            ("chain", format!("/{}", "p".repeat(4095)), Code::ChainPath),
            ("key", format!("/{}", "p".repeat(4095)), Code::KeyPath),
        ] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let (name, input) = match field {
                "profile" => (value.as_str(), files()),
                "chain" => (
                    "p",
                    ProfileInput::Files {
                        chain_file: &value,
                        key_file: "/key",
                    },
                ),
                _ => (
                    "p",
                    ProfileInput::Files {
                        chain_file: "/chain",
                        key_file: &value,
                    },
                ),
            };
            let e = b.profile(&mut arena, name, input, at(3)).unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(arena.used(), 0);
            assert_eq!(b.finish().unwrap_err(), e);
        }
        for (field, value, code) in [
            (
                "directory",
                "http://ca.example.test/".into(),
                Code::Directory,
            ),
            (
                "directory",
                format!("https://ca.example.test/{}", "a".repeat(4097 - 24)),
                Code::Directory,
            ),
            ("contact", "not-a-mailbox".into(), Code::Contact),
            (
                "contact",
                format!("{}@example.test", "a".repeat(65)),
                Code::Contact,
            ),
            ("ca", "/x//y".into(), Code::CaPath),
            ("ca", format!("/{}", "p".repeat(4095)), Code::CaPath),
        ] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let mut input = acme();
            match field {
                "directory" => input.directory = &value,
                "contact" => input.contact = &value,
                _ => input.ca_file = Some(&value),
            };
            let e = b.acme(&mut arena, input, at(4)).unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(arena.used(), 0);
            assert_eq!(b.finish().unwrap_err(), e);
        }
    }
    #[test]
    fn smaller_regions_partial_appends_and_foreign_arenas_refuse() {
        let _lock = lock();
        let mut bytes = [0; 20];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut empty = [];
        let mut excess = [Slot::EMPTY; 17];
        assert_eq!(
            Builder::new(&arena, &mut empty).unwrap_err().code,
            Code::Capacity
        );
        assert_eq!(
            Builder::new(&arena, &mut excess).unwrap_err().code,
            Code::Capacity
        );
        let mut slot = [Slot::EMPTY; 1];
        let mut b = Builder::new(&arena, &mut slot).unwrap();
        let e = b.profile(&mut arena, "p", files(), at(1)).unwrap_err();
        assert_eq!(e.code, Code::Text);
        assert!(arena.used() > 1);
        assert_eq!(b.acme(&mut arena, acme(), at(2)).unwrap_err(), e);
        assert_eq!(b.finish().unwrap_err(), e);
        let mut s = Storage::new();
        let mut other = [0; 4096];
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut foreign = text::Builder::new(&mut other).unwrap();
        {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let e = b.profile(&mut foreign, "p", files(), at(1)).unwrap_err();
            assert_eq!(e.code, Code::ForeignArena);
            assert_eq!(b.finish().unwrap_err(), e);
        }
        {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let e = b.acme(&mut foreign, acme(), at(1)).unwrap_err();
            assert_eq!(e.code, Code::ForeignArena);
            assert_eq!(b.finish().unwrap_err(), e);
        }
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        b.profile(&mut arena, "p", files(), at(1)).unwrap();
        let records = b.finish().unwrap();
        assert_eq!(
            records.view_live(&foreign).unwrap_err().code,
            Code::ForeignArena
        );
        assert_eq!(
            records.view(foreign.freeze().unwrap()).unwrap_err().code,
            Code::ForeignArena
        );
        let _ = arena.freeze().unwrap();
        let reused = text::Builder::new(&mut s.bytes).unwrap();
        assert_eq!(
            records.view_live(&reused).unwrap_err().code,
            Code::ForeignArena
        );
    }
    #[test]
    fn diagnostics_and_all_debug_wrappers_redact_sensitive_paths_and_contact() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        assert_eq!(
            format!("{b:?} {:?} {:?}", files(), acme()),
            "CertificateBuilder(<redacted>) CertificateInput(<redacted>) AcmeInput(<redacted>)"
        );
        b.profile(&mut arena, "p", ProfileInput::Acme, at(1))
            .unwrap();
        b.acme(&mut arena, acme(), at(2)).unwrap();
        let records = b.finish().unwrap();
        let view = records.view_live(&arena).unwrap();
        assert_eq!(format!("{records:?}"), "CertificateRecords(<redacted>)");
        assert_eq!(format!("{view:?}"), "CertificateView(<redacted>)");
        assert_eq!(
            format!("{:?}", view.profile(0).unwrap().unwrap()),
            "CertificateProfile(<redacted>)"
        );
        assert_eq!(
            format!("{:?}", view.acme().unwrap().unwrap()),
            "Acme(<redacted>)"
        );
        let mut slots = [Slot::EMPTY; 1];
        let mut b = Builder::new(&arena, &mut slots).unwrap();
        let e = b
            .acme(
                &mut arena,
                AcmeInput {
                    contact: "private-invalid",
                    ..acme()
                },
                at(3),
            )
            .unwrap_err();
        assert!(!format!("{e:?} {e}").contains("private"));
        assert!(std::error::Error::source(&e).is_none());
    }
}
