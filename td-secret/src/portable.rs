//! Bounded portable envelope. Token authorization and persistence are adapters.

use super::crypto;
use std::collections::BTreeSet;
use std::io::Read;

type Result<T> = std::result::Result<T, String>;

const MAGIC: &[u8; 8] = b"TDVAULT1";
const WRAP: &[u8] = b"td-secret/portable/wrap/v1";
const SLOT: &[u8] = b"td-secret/portable/slot/v1\0";
const BODY: &[u8] = b"td-secret/portable/body/v1\0";
const MAX_ENTRIES: usize = 1024;
const MAX_TITLE: usize = 512;
const MAX_BODY: usize = 64 * 1024;
const MAX_PLAIN: usize = 4 * 1024 * 1024;
const MAX_SLOTS: usize = 8;
const MAX_CREDENTIAL: usize = 1024;
const MAX_ENVELOPE: usize = MAX_PLAIN + 16 + 65 + MAX_SLOTS * 1119;

// Neither secret owner implements Debug or Clone.
pub(super) struct Secret32([u8; 32]);

impl Drop for Secret32 {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct Plaintext(Vec<u8>);

impl Drop for Plaintext {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Primary = 1,
    Backup = 2,
}

impl Role {
    fn decode(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Primary),
            2 => Ok(Self::Backup),
            _ => Err("invalid portable key role".into()),
        }
    }
}

pub(super) struct Entry {
    pub id: [u8; 16],
    pub revision: u64,
    title: String,
    body: Vec<u8>,
}

impl Entry {
    pub fn new(id: [u8; 16], revision: u64, title: String, body: Vec<u8>) -> Self {
        Self {
            id,
            revision,
            title,
            body,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn replace_text(&mut self, title: String, body: Vec<u8>) {
        self.clear_text();
        self.title = title;
        self.body = body;
    }

    fn clear_text(&mut self) {
        let mut title = std::mem::take(&mut self.title).into_bytes();
        title.fill(0);
        self.body.fill(0);
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.clear_text();
    }
}

pub(super) struct Notebook {
    pub entries: Vec<Entry>,
}

impl Notebook {
    fn validate(&self) -> Result<usize> {
        if self.entries.len() > MAX_ENTRIES {
            return Err("portable vault has too many entries".into());
        }
        let mut ids = BTreeSet::new();
        let mut titles = BTreeSet::new();
        let mut size = 4usize;
        for entry in &self.entries {
            if entry.revision == 0
                || entry.title.is_empty()
                || entry.title.len() > MAX_TITLE
                || entry.title.chars().any(char::is_control)
                || entry.body.len() > MAX_BODY
                || std::str::from_utf8(&entry.body).is_err()
                || !ids.insert(entry.id)
                || !titles.insert(entry.title.as_str())
            {
                return Err("invalid or duplicate portable entry".into());
            }
            size = size
                .checked_add(32 + entry.title.len() + entry.body.len())
                .filter(|size| *size <= MAX_PLAIN)
                .ok_or("portable notebook is oversized")?;
        }
        Ok(size)
    }

    fn encode(&self) -> Result<Plaintext> {
        let size = self.validate()?;
        let mut out = Plaintext(Vec::with_capacity(size));
        put_u32(&mut out.0, self.entries.len())?;
        for entry in &self.entries {
            out.0.extend_from_slice(&entry.id);
            out.0.extend_from_slice(&entry.revision.to_be_bytes());
            put_field(&mut out.0, entry.title.as_bytes())?;
            put_field(&mut out.0, &entry.body)?;
        }
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_PLAIN {
            return Err("portable notebook is oversized".into());
        }
        let mut reader = Reader(bytes);
        let count = reader.count(MAX_ENTRIES)?;
        let mut notebook = Self {
            entries: Vec::new(),
        };
        for _ in 0..count {
            let id = reader.array()?;
            let revision = u64::from_be_bytes(reader.array()?);
            let title = std::str::from_utf8(reader.field(MAX_TITLE)?)
                .map_err(|_| "portable title is not UTF-8")?;
            let body = reader.field(MAX_BODY)?;
            std::str::from_utf8(body).map_err(|_| "portable body is not UTF-8")?;
            notebook.entries.push(Entry {
                id,
                revision,
                title: title.to_owned(),
                body: body.to_vec(),
            });
        }
        reader.end()?;
        notebook.validate()?;
        Ok(notebook)
    }
}

// Produced only by the future proved enrollment adapter, never an app API.
pub(super) struct Protector {
    pub role: Role,
    pub credential: Vec<u8>,
    pub salt: [u8; 32],
    pub secret: Secret32,
}

struct Slot {
    role: Role,
    credential: Vec<u8>,
    salt: [u8; 32],
    nonce: [u8; 12],
    wrapped: [u8; 48],
}

impl Slot {
    fn context(&self, id: &[u8; 32]) -> Result<Vec<u8>> {
        let mut out = SLOT.to_vec();
        out.extend_from_slice(id);
        self.metadata(&mut out)?;
        Ok(out)
    }

    fn metadata(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.credential.is_empty() || self.credential.len() > MAX_CREDENTIAL {
            return Err("invalid portable credential length".into());
        }
        out.push(self.role as u8);
        let size = u16::try_from(self.credential.len())
            .map_err(|_| "portable credential length overflow")?;
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(&self.credential);
        out.extend_from_slice(&self.salt);
        Ok(())
    }
}

pub(super) struct LockedVault {
    bytes: Vec<u8>,
    id: [u8; 32],
    revision: u64,
    slots: Vec<Slot>,
    nonce: [u8; 12],
    body_offset: usize,
}

pub(super) struct OpenVault {
    master: Secret32,
    fingerprint: [u8; 32],
    pub notebook: Notebook,
}

impl LockedVault {
    pub fn create(
        notebook: &Notebook,
        protectors: Vec<Protector>,
        random: &mut impl Read,
    ) -> Result<Self> {
        // Complete cheap admission before entropy or cryptographic work.
        notebook.validate()?;
        if !(2..=MAX_SLOTS).contains(&protectors.len()) {
            return Err("portable vault requires two through eight keys".into());
        }
        // Sort references so token secrets remain in their clearing owners.
        let mut ordered: Vec<&Protector> = protectors.iter().collect();
        ordered.sort_by(|a, b| a.credential.cmp(&b.credential));
        validate_keys(ordered.iter().map(|p| (p.role, p.credential.as_slice())))?;
        let id = random_array(random)?;
        let master = Secret32(random_array(random)?);
        let mut slots = Vec::new();
        for protector in ordered {
            let mut slot = Slot {
                role: protector.role,
                credential: protector.credential.clone(),
                salt: protector.salt,
                nonce: random_array(random)?,
                wrapped: [0; 48],
            };
            let key = Secret32(crypto::hkdf(&protector.secret.0, &id, WRAP));
            let wrapped = crypto::seal(&key.0, &slot.nonce, &slot.context(&id)?, &master.0);
            slot.wrapped = wrapped
                .as_slice()
                .try_into()
                .map_err(|_| "invalid wrapped portable key length")?;
            slots.push(slot);
        }
        Self::seal(&id, 1, &slots, &master, notebook, random)
    }

    fn seal(
        id: &[u8; 32],
        revision: u64,
        slots: &[Slot],
        master: &Secret32,
        notebook: &Notebook,
        random: &mut impl Read,
    ) -> Result<Self> {
        let plaintext = notebook.encode()?;
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(id);
        out.extend_from_slice(&revision.to_be_bytes());
        out.push(u8::try_from(slots.len()).map_err(|_| "portable key count overflow")?);
        for slot in slots {
            slot.metadata(&mut out)?;
            out.extend_from_slice(&slot.nonce);
            out.extend_from_slice(&slot.wrapped);
        }
        let nonce = random_array(random)?;
        out.extend_from_slice(&nonce);
        put_u32(&mut out, plaintext.0.len() + 16)?;
        let mut aad = BODY.to_vec();
        aad.extend_from_slice(&out);
        let key = Secret32(crypto::hkdf(&master.0, id, BODY));
        out.extend_from_slice(&crypto::seal(&key.0, &nonce, &aad, &plaintext.0));
        Self::decode(&out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_ENVELOPE {
            return Err("portable envelope is oversized".into());
        }
        let mut reader = Reader(bytes);
        if reader.take(8)? != MAGIC {
            return Err("unsupported portable vault format".into());
        }
        let id = reader.array()?;
        let revision = u64::from_be_bytes(reader.array()?);
        if revision == 0 {
            return Err("invalid portable vault revision".into());
        }
        let [count] = reader.array()?;
        if !(2..=MAX_SLOTS).contains(&usize::from(count)) {
            return Err("portable vault requires two through eight keys".into());
        }
        let mut slots = Vec::new();
        for _ in 0..count {
            let [role] = reader.array()?;
            let role = Role::decode(role)?;
            let size = usize::from(u16::from_be_bytes(reader.array()?));
            if size == 0 || size > MAX_CREDENTIAL {
                return Err("invalid portable credential length".into());
            }
            slots.push(Slot {
                role,
                credential: reader.take(size)?.to_vec(),
                salt: reader.array()?,
                nonce: reader.array()?,
                wrapped: reader.array()?,
            });
        }
        validate_keys(slots.iter().map(|s| (s.role, s.credential.as_slice())))?;
        let nonce = reader.array()?;
        let size = reader.count(MAX_PLAIN + 16)?;
        if size < 20 {
            return Err("truncated portable notebook".into());
        }
        let body_offset = bytes.len() - reader.0.len();
        reader.take(size)?;
        reader.end()?;
        Ok(Self {
            bytes: bytes.to_vec(),
            id,
            revision,
            slots,
            nonce,
            body_offset,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    // The secret must come from the enrolled UV hmac-secret adapter. This
    // primitive proves ciphertext possession, not a fresh presented operation.
    pub fn open(&self, credential: &[u8], secret: &Secret32) -> Result<OpenVault> {
        let slot = self
            .slots
            .iter()
            .find(|s| s.credential == credential)
            .ok_or("portable credential is not enrolled")?;
        let key = Secret32(crypto::hkdf(&secret.0, &self.id, WRAP));
        let raw = Plaintext(crypto::open(
            &key.0,
            &slot.nonce,
            &slot.context(&self.id)?,
            &slot.wrapped,
        )?);
        let master = Secret32(
            raw.0
                .as_slice()
                .try_into()
                .map_err(|_| "invalid portable vault key length")?,
        );
        let key = Secret32(crypto::hkdf(&master.0, &self.id, BODY));
        let mut aad = BODY.to_vec();
        aad.extend_from_slice(
            self.bytes
                .get(..self.body_offset)
                .ok_or("invalid portable header extent")?,
        );
        let sealed = self
            .bytes
            .get(self.body_offset..)
            .ok_or("invalid portable body extent")?;
        let raw = Plaintext(crypto::open(&key.0, &self.nonce, &aad, sealed)?);
        let notebook = Notebook::decode(&raw.0)?;
        Ok(OpenVault {
            master,
            fingerprint: crypto::digest(&self.bytes),
            notebook,
        })
    }

    pub fn revise(
        &self,
        opened: &OpenVault,
        notebook: &Notebook,
        random: &mut impl Read,
    ) -> Result<Self> {
        if opened.fingerprint != crypto::digest(&self.bytes) {
            return Err("portable session belongs to another vault revision".into());
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("portable revision exhausted")?;
        Self::seal(
            &self.id,
            revision,
            &self.slots,
            &opened.master,
            notebook,
            random,
        )
    }
}

fn validate_keys<'a>(keys: impl Iterator<Item = (Role, &'a [u8])>) -> Result<()> {
    let mut previous: Option<&[u8]> = None;
    let mut primary = 0;
    for (role, credential) in keys {
        if credential.is_empty()
            || credential.len() > MAX_CREDENTIAL
            || previous.is_some_and(|p| p >= credential)
        {
            return Err("invalid, duplicate or unordered portable credentials".into());
        }
        primary += usize::from(role == Role::Primary);
        previous = Some(credential);
    }
    if primary != 1 {
        return Err("portable vault requires exactly one primary key".into());
    }
    Ok(())
}

fn random_array<const N: usize>(random: &mut impl Read) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    if random.read_exact(&mut bytes).is_err() {
        bytes.fill(0);
        return Err("portable vault entropy unavailable".into());
    }
    Ok(bytes)
}

fn put_u32(out: &mut Vec<u8>, size: usize) -> Result<()> {
    out.extend_from_slice(
        &u32::try_from(size)
            .map_err(|_| "portable length overflow")?
            .to_be_bytes(),
    );
    Ok(())
}

fn put_field(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    put_u32(out, bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        let (bytes, rest) = self
            .0
            .split_at_checked(size)
            .ok_or("truncated portable vault")?;
        self.0 = rest;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| "invalid portable field extent".into())
    }

    fn count(&mut self, limit: usize) -> Result<usize> {
        let size = usize::try_from(u32::from_be_bytes(self.array()?))
            .map_err(|_| "portable length overflow")?;
        if size > limit {
            return Err("portable field exceeds its limit".into());
        }
        Ok(size)
    }

    fn field(&mut self, limit: usize) -> Result<&'a [u8]> {
        let size = self.count(limit)?;
        self.take(size)
    }

    fn end(self) -> Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err("trailing portable vault bytes".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn random() -> Cursor<Vec<u8>> {
        Cursor::new((0u8..=255).cycle().take(1024).collect())
    }

    fn notebook() -> Notebook {
        Notebook {
            entries: vec![Entry {
                id: [0x33; 16],
                revision: 1,
                title: "Email/Personal".into(),
                body: b"username: alice\npassword: example\r\n".to_vec(),
            }],
        }
    }

    fn protectors() -> Vec<Protector> {
        vec![
            Protector {
                role: Role::Primary,
                credential: b"primary".to_vec(),
                salt: [0xa1; 32],
                secret: Secret32([0x11; 32]),
            },
            Protector {
                role: Role::Backup,
                credential: b"backup".to_vec(),
                salt: [0xb2; 32],
                secret: Secret32([0x22; 32]),
            },
        ]
    }

    fn fixture() -> LockedVault {
        LockedVault::create(&notebook(), protectors(), &mut random()).unwrap()
    }

    fn both_open(vault: &LockedVault, expected: &[u8]) {
        for (credential, key) in [(b"primary".as_slice(), 0x11), (b"backup".as_slice(), 0x22)] {
            let opened = vault.open(credential, &Secret32([key; 32])).unwrap();
            let entry = &opened.notebook.entries[0];
            assert_eq!(entry.body(), expected);
            assert_eq!(entry.id, [0x33; 16]);
            assert_eq!(entry.revision, 1);
            assert_eq!(entry.title(), "Email/Personal");
            let slot = vault
                .slots
                .iter()
                .find(|s| s.credential == credential)
                .unwrap();
            assert!(
                slot.role
                    == if key == 0x11 {
                        Role::Primary
                    } else {
                        Role::Backup
                    }
            );
        }
    }

    #[test]
    fn primary_and_backup_independently_open_the_exported_bytes() {
        let created = fixture();
        let exported = created.bytes().to_vec();
        drop(created);
        let imported = LockedVault::decode(&exported).unwrap();
        both_open(&imported, &notebook().entries[0].body);
        assert!(!exported.windows(14).any(|w| w == b"Email/Personal"));
        assert!(!exported.windows(8).any(|w| w == b"password"));
        assert!(imported.open(b"primary", &Secret32([0x22; 32])).is_err());
        assert!(imported.open(b"missing", &Secret32([0x11; 32])).is_err());
    }

    #[test]
    fn every_byte_is_structurally_checked_or_authenticated_by_both_keys() {
        let vault = fixture();
        for index in 0..vault.bytes().len() {
            let mut modified = vault.bytes().to_vec();
            modified[index] ^= 1;
            if let Ok(parsed) = LockedVault::decode(&modified) {
                assert!(
                    parsed.open(b"primary", &Secret32([0x11; 32])).is_err(),
                    "primary accepted byte {index}"
                );
                assert!(
                    parsed.open(b"backup", &Secret32([0x22; 32])).is_err(),
                    "backup accepted byte {index}"
                );
            }
        }
    }

    #[test]
    fn all_truncations_and_trailing_data_are_refused_before_unlock() {
        let vault = fixture();
        for length in 0..vault.bytes().len() {
            assert!(
                LockedVault::decode(&vault.bytes()[..length]).is_err(),
                "length {length}"
            );
        }
        let mut trailing = vault.bytes().to_vec();
        trailing.push(0);
        assert!(LockedVault::decode(&trailing).is_err());
        assert!(LockedVault::decode(&vec![0; MAX_ENVELOPE + 1]).is_err());
        let mut wrong = vault.bytes().to_vec();
        wrong[48] = 255;
        assert!(LockedVault::decode(&wrong).is_err());
        wrong = vault.bytes().to_vec();
        wrong[50..52].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(LockedVault::decode(&wrong).is_err());
        wrong = vault.bytes().to_vec();
        wrong[vault.body_offset - 4..vault.body_offset].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(LockedVault::decode(&wrong).is_err());
    }

    #[test]
    fn invalid_key_tables_refuse_before_entropy() {
        let mut cases = vec![Vec::new()];
        let mut single = protectors();
        single.pop();
        cases.push(single);
        let mut duplicate = protectors();
        duplicate[1].credential = b"primary".to_vec();
        cases.push(duplicate);
        let mut no_primary = protectors();
        no_primary[0].role = Role::Backup;
        cases.push(no_primary);
        let mut two_primary = protectors();
        two_primary[1].role = Role::Primary;
        cases.push(two_primary);
        let mut long = protectors();
        long[0].credential = vec![1; MAX_CREDENTIAL + 1];
        cases.push(long);
        let mut empty = protectors();
        empty[0].credential.clear();
        cases.push(empty);
        for keys in cases {
            let mut entropy = random();
            assert!(LockedVault::create(&notebook(), keys, &mut entropy).is_err());
            assert_eq!(entropy.position(), 0);
        }
        assert!(validate_keys(
            [
                (Role::Primary, b"z".as_slice()),
                (Role::Backup, b"a".as_slice())
            ]
            .into_iter()
        )
        .is_err());
    }

    #[test]
    fn invalid_entries_and_plaintext_extents_are_refused() {
        let mut cases = Vec::new();
        let mut n = notebook();
        n.entries[0].title.clear();
        cases.push(n);
        let mut n = notebook();
        n.entries[0].title = "a\nb".into();
        cases.push(n);
        let mut n = notebook();
        n.entries[0].title = "a".repeat(MAX_TITLE + 1);
        cases.push(n);
        let mut n = notebook();
        n.entries[0].body = vec![b'a'; MAX_BODY + 1];
        cases.push(n);
        let mut n = notebook();
        n.entries[0].body = vec![0xff];
        cases.push(n);
        let mut n = notebook();
        n.entries[0].revision = 0;
        cases.push(n);
        let mut n = notebook();
        n.entries.push(notebook().entries.remove(0));
        cases.push(n);
        let mut n = notebook();
        let mut e = notebook().entries.remove(0);
        e.id = [8; 16];
        n.entries.push(e);
        cases.push(n);
        for n in cases {
            let mut entropy = random();
            assert!(LockedVault::create(&n, protectors(), &mut entropy).is_err());
            assert_eq!(entropy.position(), 0);
        }
        let encoded = notebook().encode().unwrap();
        for size in 0..encoded.0.len() {
            assert!(Notebook::decode(&encoded.0[..size]).is_err());
        }
        assert!(Notebook::decode(&u32::MAX.to_be_bytes()).is_err());
        let mut trailing = encoded.0.clone();
        trailing.push(0);
        assert!(Notebook::decode(&trailing).is_err());
        let mut invalid_body = encoded.0.clone();
        invalid_body[..4].copy_from_slice(&2u32.to_be_bytes());
        *invalid_body.last_mut().unwrap() = 0xff;
        assert!(
            matches!(Notebook::decode(&invalid_body), Err(error) if error == "portable body is not UTF-8")
        );
    }

    #[test]
    fn revision_requires_the_exact_opened_snapshot_and_preserves_old_bytes() {
        let old = fixture();
        let original = old.bytes().to_vec();
        let opened = old.open(b"primary", &Secret32([0x11; 32])).unwrap();
        let mut changed = notebook();
        changed.entries[0].replace_text("Email/Personal".into(), "café\r\n\n".as_bytes().to_vec());
        let mut entropy = random();
        entropy.set_position(100);
        let revised = old.revise(&opened, &changed, &mut entropy).unwrap();
        assert_eq!(revised.revision, 2);
        both_open(&revised, "café\r\n\n".as_bytes());
        assert_ne!(old.nonce, revised.nonce);
        assert_eq!(old.bytes(), original);
        both_open(&old, &notebook().entries[0].body);
        assert!(revised.revise(&opened, &changed, &mut random()).is_err());
        let mut other_entropy = random();
        other_entropy.set_position(10);
        let other = LockedVault::create(&notebook(), protectors(), &mut other_entropy).unwrap();
        assert!(other.revise(&opened, &changed, &mut random()).is_err());
        assert!(old
            .revise(&opened, &changed, &mut Cursor::new(Vec::<u8>::new()))
            .is_err());
        assert_eq!(old.bytes(), original);
    }

    #[test]
    fn entropy_failure_at_every_boundary_returns_no_envelope() {
        for count in 0..100 {
            let mut entropy = Cursor::new(vec![7; count]);
            assert!(
                LockedVault::create(&notebook(), protectors(), &mut entropy).is_err(),
                "count {count}"
            );
        }
    }

    #[test]
    fn empty_notebook_and_empty_entry_body_are_valid() {
        let empty = Notebook {
            entries: Vec::new(),
        };
        let vault = LockedVault::create(&empty, protectors(), &mut random()).unwrap();
        assert!(vault
            .open(b"backup", &Secret32([0x22; 32]))
            .unwrap()
            .notebook
            .entries
            .is_empty());
        let mut n = notebook();
        n.entries[0].body.clear();
        let vault = LockedVault::create(&n, protectors(), &mut random()).unwrap();
        both_open(&vault, b"");
    }

    #[test]
    fn maximum_plaintext_and_maximum_key_table_roundtrip() {
        let mut n = Notebook {
            entries: Vec::new(),
        };
        let mut remaining = MAX_PLAIN - 4;
        let mut number = 0u128;
        while remaining > 0 {
            let title = format!("entry-{number:04}");
            let overhead = 32 + title.len();
            let body_size = (remaining - overhead).min(MAX_BODY);
            n.entries.push(Entry {
                id: number.to_be_bytes(),
                revision: 1,
                title,
                body: vec![b'x'; body_size],
            });
            remaining -= overhead + body_size;
            number += 1;
        }
        assert_eq!(n.validate().unwrap(), MAX_PLAIN);
        let mut keys = protectors();
        for number in 2..MAX_SLOTS {
            keys.push(Protector {
                role: Role::Backup,
                credential: vec![number as u8; MAX_CREDENTIAL],
                salt: [number as u8; 32],
                secret: Secret32([number as u8; 32]),
            });
        }
        let vault = LockedVault::create(&n, keys, &mut random()).unwrap();
        assert!(vault.bytes().len() <= MAX_ENVELOPE);
        let opened = vault.open(b"backup", &Secret32([0x22; 32])).unwrap();
        assert_eq!(opened.notebook.validate().unwrap(), MAX_PLAIN);
        n.entries.last_mut().unwrap().body.push(b'x');
        assert!(n.validate().is_err());
    }

    #[test]
    fn entry_count_bounds_and_revision_exhaustion() {
        let mut n = Notebook {
            entries: (0..MAX_ENTRIES)
                .map(|i| {
                    Entry::new(
                        (i as u128).to_be_bytes(),
                        1,
                        format!("entry-{i}"),
                        Vec::new(),
                    )
                })
                .collect(),
        };
        let encoded = n.encode().unwrap();
        assert_eq!(
            Notebook::decode(&encoded.0).unwrap().entries.len(),
            MAX_ENTRIES
        );
        n.entries
            .push(Entry::new([0xff; 16], 1, "overflow".into(), Vec::new()));
        assert!(n.validate().is_err());
        assert!(Notebook::decode(&((MAX_ENTRIES + 1) as u32).to_be_bytes()).is_err());
        let vault = fixture();
        let opened = vault.open(b"primary", &Secret32([0x11; 32])).unwrap();
        let exhausted = LockedVault::seal(
            &vault.id,
            u64::MAX,
            &vault.slots,
            &opened.master,
            &opened.notebook,
            &mut random(),
        )
        .unwrap();
        let opened = exhausted.open(b"primary", &Secret32([0x11; 32])).unwrap();
        let mut entropy = random();
        assert!(
            matches!(exhausted.revise(&opened, &opened.notebook, &mut entropy),
            Err(error) if error == "portable revision exhausted")
        );
        assert_eq!(entropy.position(), 0);
    }

    #[test]
    fn cross_vault_slot_substitution_refuses_both_keys() {
        let first = fixture();
        let mut entropy = random();
        entropy.set_position(10);
        let second = LockedVault::create(&notebook(), protectors(), &mut entropy).unwrap();
        assert_ne!(first.id, second.id);
        let slot_size = 1 + 2 + b"backup".len() + 32 + 12 + 48;
        let mut spliced = first.bytes().to_vec();
        spliced[49..49 + slot_size].copy_from_slice(&second.bytes()[49..49 + slot_size]);
        let parsed = LockedVault::decode(&spliced).unwrap();
        assert!(parsed.open(b"primary", &Secret32([0x11; 32])).is_err());
        assert!(parsed.open(b"backup", &Secret32([0x22; 32])).is_err());
    }

    #[test]
    fn independent_openssl_and_python_envelope_vector() {
        // Generated by tests/portable_vector.py with host OpenSSL 3.5.7.
        let hex = concat!(
            "54445641554c5431000102030405060708090a0b0c0d0e0f101112131415161718191a1b",
            "1c1d1e1f0000000000000001020200066261636b7570b2b2b2b2b2b2b2b2b2b2b2b2b2b2",
            "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2404142434445464748494a4bcb606d2baece",
            "5eee1f036a13499c6ee1a212f288f433c1f695635c48e0bab844fdf1e50f3c0b628153b3",
            "30b8e4b4b6830100077072696d617279a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
            "a1a1a1a1a1a1a1a1a1a1a1a14c4d4e4f505152535455565742cc6ae8451957c37c0ec9ad",
            "9f91d8a8ca4846ec136281dd06c51ba96fb433109204c5b79492788aea87cdd0ed59b31e",
            "58595a5b5c5d5e5f6061626300000065462f990168d2b1ca4526a2a2a2661088c8c6cd79",
            "8526125ef02eddf979402b9f7c30c8123477d35106251a1bfa8e3384d933ffe4610c5b25",
            "e0d613037c44feda3b8be6b8035aaad7b6307a0a323a2a857c1bfdf076eacc3b7347ef80",
            "d02ba6b187a9d6d778",
        );
        let expected: Vec<u8> = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(fixture().bytes(), expected);
        both_open(
            &LockedVault::decode(&expected).unwrap(),
            &notebook().entries[0].body,
        );
    }
}
