//! Portable vault lifecycle over proved token results. Device admission,
//! prompts and directory acquisition are adapter duties.

use super::notebook::EntryError;
use super::storage::{PublishError, Snapshot, Store};
use super::{
    equal32, LockedVault, Notebook, OpenVault, Protector, Role, Secret32, UnlockHint,
    VerificationKey, MAX_SLOTS,
};
use crate::crypto;
use crate::fido_pin::Pin;
use crate::fido_transaction::{
    self as transaction, Assertion, Channel, Enrollment, PinPurpose, Transaction,
};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

const OPERATION: &[u8] = b"td-secret/portable/operation/v1\0";

/// What one token presentation authorizes. Labels carry no notebook content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    CreatePrimary = 1,
    CreateBackup = 2,
    Unlock = 3,
    Save = 4,
    AuthorizeAddKey = 5,
    AddKey = 6,
    AuthorizeReplaceKey = 7,
    ReplaceKey = 8,
    Import = 9,
}

impl Purpose {
    pub fn label(self) -> &'static str {
        match self {
            Self::CreatePrimary => "create a portable vault: enroll the primary key",
            Self::CreateBackup => "create a portable vault: enroll the separate backup key",
            Self::Unlock => "unlock the portable vault for browsing",
            Self::Save => "save a new portable vault revision",
            Self::AuthorizeAddKey => "authorize adding a key with an enrolled key",
            Self::AddKey => "enroll the added key",
            Self::AuthorizeReplaceKey => "authorize key replacement with a retained key",
            Self::ReplaceKey => "enroll the replacement key",
            Self::Import => "import a portable vault copy into this empty location",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Create = 1,
    Prove = 2,
    Repeat = 3,
    Assert = 4,
}

/// Fresh client-data hash bound to the purpose, phase and exact operation.
fn challenge(
    purpose: Purpose,
    phase: Phase,
    binding: &[u8],
    random: &mut impl Read,
) -> Result<[u8; 32], Error> {
    let mut nonce = [0; 32];
    random
        .read_exact(&mut nonce)
        .map_err(|_| Error::Refused("portable operation entropy unavailable".into()))?;
    let mut input = OPERATION.to_vec();
    input.extend_from_slice(&[purpose as u8, phase as u8]);
    let size = u32::try_from(binding.len())
        .map_err(|_| Error::Refused("portable operation binding overflow".into()))?;
    input.extend_from_slice(&size.to_be_bytes());
    input.extend_from_slice(binding);
    input.extend_from_slice(&nonce);
    Ok(crypto::digest(&input))
}

fn binding(vault: &LockedVault, detail: &[u8]) -> Vec<u8> {
    let mut out = vault.id().to_vec();
    out.extend_from_slice(&vault.revision().to_be_bytes());
    out.extend_from_slice(detail);
    out
}

pub(super) struct EnrollRequest<'a> {
    pub challenge: [u8; 32],
    pub proof_challenge: [u8; 32],
    pub user: [u8; 32],
    pub salt: [u8; 32],
    pub excluded: &'a [&'a [u8]],
}

pub(super) struct AssertRequest<'a> {
    pub credential: &'a [u8],
    pub key: &'a VerificationKey,
    pub salt: &'a [u8; 32],
    pub challenge: [u8; 32],
}

/// One verified creation plus proof; the secret is the UV hmac-secret output.
pub(super) struct Enrolled {
    pub credential: Vec<u8>,
    pub key: VerificationKey,
    pub salt: [u8; 32],
    pub secret: Secret32,
    pub counter: u32,
}

pub(super) struct Asserted {
    pub secret: Secret32,
    pub counter: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TokenError {
    /// No admitted device or channel; nothing was sent.
    Unavailable,
    /// The host's device policy denies this account a connected token.
    Denied,
    /// More than one token is connected; nothing was sent.
    Several,
    /// The host adapter refused before discovering a token.
    Host(&'static str),
    Failed(transaction::Error),
}

/// What the operator is asked to present: the operation, the role of the
/// key, and for an enrolled key its credential, so a prompt can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Presented<'a> {
    pub purpose: Purpose,
    pub role: Role,
    pub credential: Option<&'a [u8]>,
}

/// Each call admits one fresh channel, presents one key, and returns only a
/// verified result. Implementations never retry or select another protocol.
pub(super) trait Tokens {
    fn enroll(
        &mut self,
        presented: Presented<'_>,
        request: EnrollRequest<'_>,
    ) -> Result<Enrolled, TokenError>;
    fn assert(
        &mut self,
        presented: Presented<'_>,
        request: AssertRequest<'_>,
    ) -> Result<Asserted, TokenError>;

    /// Whether the owner revoked the operation after its last token
    /// exchange, as a lock does; checked once more before publication. A
    /// revocation after that check cannot stop the publication begun.
    fn revoked(&self) -> bool {
        false
    }
}

fn check_revoked(tokens: &(impl Tokens + ?Sized)) -> Result<(), Error> {
    if tokens.revoked() {
        return Err(Error::Token(TokenError::Failed(
            transaction::Error::Interrupted(crate::fido_device::Interruption::Cancelled),
        )));
    }
    Ok(())
}

/// The production adapter over the owned transaction runner.
pub(super) struct Hardware<O, P, E> {
    pub open: O,
    pub prompt: P,
    pub entropy: E,
}

impl<C, O, P, E> Tokens for Hardware<O, P, E>
where
    C: Channel,
    O: FnMut(Presented<'_>) -> Result<C, TokenError>,
    P: FnMut(Presented<'_>, PinPurpose) -> Result<Pin, String>,
    E: FnMut(&mut [u8]) -> Result<(), String>,
{
    fn enroll(
        &mut self,
        presented: Presented<'_>,
        request: EnrollRequest<'_>,
    ) -> Result<Enrolled, TokenError> {
        let channel = (self.open)(presented)?;
        let prompt = &mut self.prompt;
        let enrolled = Transaction::new(channel)
            .and_then(|transaction| {
                transaction.enroll(
                    Enrollment {
                        challenge: request.challenge,
                        user: request.user,
                        proof_challenge: request.proof_challenge,
                        salt: request.salt,
                        excluded: request.excluded,
                    },
                    &mut |pin| prompt(presented, pin),
                    &mut self.entropy,
                )
            })
            .map_err(TokenError::Failed)?;
        let key = VerificationKey::from_cose(enrolled.cose())
            .map_err(|error| TokenError::Failed(transaction::Error::Protocol(error)))?;
        if enrolled.salt() != &request.salt {
            return Err(TokenError::Failed(transaction::Error::Protocol(
                "enrollment proof used another salt".into(),
            )));
        }
        Ok(Enrolled {
            credential: enrolled.id().to_vec(),
            key,
            salt: request.salt,
            secret: secret(enrolled.output().bytes())?,
            counter: enrolled.output().info.counter,
        })
    }

    fn assert(
        &mut self,
        presented: Presented<'_>,
        request: AssertRequest<'_>,
    ) -> Result<Asserted, TokenError> {
        let key = request
            .key
            .public_key()
            .map_err(|error| TokenError::Failed(transaction::Error::Protocol(error)))?;
        let channel = (self.open)(presented)?;
        let prompt = &mut self.prompt;
        let output = Transaction::new(channel)
            .and_then(|transaction| {
                transaction.assertion(
                    Assertion {
                        credential: request.credential,
                        key,
                        challenge: request.challenge,
                        salt: *request.salt,
                    },
                    &mut |pin| prompt(presented, pin),
                    &mut self.entropy,
                )
            })
            .map_err(TokenError::Failed)?;
        Ok(Asserted {
            secret: secret(output.bytes())?,
            counter: output.info.counter,
        })
    }
}

fn secret(bytes: &[u8]) -> Result<Secret32, TokenError> {
    let mut owner = Secret32([0; 32]);
    if bytes.len() != owner.0.len() {
        return Err(TokenError::Failed(transaction::Error::Protocol(
            "invalid portable token secret length".into(),
        )));
    }
    owner.0.copy_from_slice(bytes);
    Ok(owner)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Error {
    Token(TokenError),
    /// Store admission, lock or read failure before any change.
    Store(String),
    /// The committed vault is absent, present, or changed contrary to the request.
    State(&'static str),
    /// Verification or structural refusal; nothing was published.
    Refused(String),
    /// An entry change refused before any token was presented.
    Entry(EntryError),
    /// Publication was attempted with an unknown result. Lock and unlock again.
    Uncertain,
}

impl From<TokenError> for Error {
    fn from(error: TokenError) -> Self {
        Self::Token(error)
    }
}

/// A directory descriptor and owner admitted by the adapter's path policy.
pub(super) struct Directory {
    pub file: File,
    pub owner: u32,
}

impl Directory {
    fn store(&self) -> Result<Store, Error> {
        let file = self
            .file
            .try_clone()
            .map_err(|_| Error::Store("retain portable directory".into()))?;
        Store::open(file, self.owner).map_err(Error::Store)
    }

    // The exclusive transaction ends before any token is presented.
    fn snapshot(&self) -> Result<Snapshot, Error> {
        self.store()?.load().map_err(Error::Store)
    }

    // The directory's identity, which a session keeps so its writes cannot
    // be pointed at another location holding the same bytes.
    fn place(&self) -> Result<(u64, u64), Error> {
        let meta = self
            .file
            .metadata()
            .map_err(|_| Error::Store("inspect portable directory".into()))?;
        Ok((meta.dev(), meta.ino()))
    }
}

/// Public metadata of an enrolled key, for selecting which token to present.
pub(super) struct KeyInfo {
    pub role: Role,
    pub credential: Vec<u8>,
}

fn key_info(hint: UnlockHint<'_>) -> KeyInfo {
    KeyInfo {
        role: hint.role,
        credential: hint.credential.to_vec(),
    }
}

/// Whether a vault is committed in `directory`.
pub(super) fn exists(directory: &Directory) -> Result<bool, Error> {
    Ok(directory.snapshot()?.vault().is_some())
}

/// Lists the keys of the committed vault without authenticating it.
pub(super) fn keys(directory: &Directory) -> Result<Vec<KeyInfo>, Error> {
    let snapshot = directory.snapshot()?;
    let vault = snapshot
        .vault()
        .ok_or(Error::State("no portable vault exists"))?;
    Ok(vault.unlock_hints().map(key_info).collect())
}

fn random32(random: &mut impl Read) -> Result<[u8; 32], Error> {
    let mut bytes = [0; 32];
    random
        .read_exact(&mut bytes)
        .map_err(|_| Error::Refused("portable entropy unavailable".into()))?;
    Ok(bytes)
}

// Counters may both be zero; otherwise a later assertion must increase.
fn advanced(before: u32, after: u32) -> bool {
    (before == 0 && after == 0) || after > before
}

/// Creation and proof, then an independent repeat assertion that must
/// recover the identical secret with an advancing counter.
fn enroll_proved(
    tokens: &mut impl Tokens,
    purpose: Purpose,
    role: Role,
    detail: &[u8],
    excluded: &[&[u8]],
    random: &mut impl Read,
) -> Result<(Protector, u32), Error> {
    let salt = random32(random)?;
    let enrolled = tokens.enroll(
        Presented {
            purpose,
            role,
            credential: None,
        },
        EnrollRequest {
            challenge: challenge(purpose, Phase::Create, detail, random)?,
            proof_challenge: challenge(purpose, Phase::Prove, detail, random)?,
            user: random32(random)?,
            salt,
            excluded,
        },
    )?;
    if enrolled.salt != salt || excluded.contains(&enrolled.credential.as_slice()) {
        return Err(Error::Refused("enrolled portable key is not fresh".into()));
    }
    let repeat = tokens.assert(
        Presented {
            purpose,
            role,
            credential: Some(&enrolled.credential),
        },
        AssertRequest {
            credential: &enrolled.credential,
            key: &enrolled.key,
            salt: &enrolled.salt,
            challenge: challenge(purpose, Phase::Repeat, detail, random)?,
        },
    )?;
    if !equal32(&enrolled.secret, &repeat.secret) {
        return Err(Error::Refused(
            "portable key secret was not repeatable".into(),
        ));
    }
    if !advanced(enrolled.counter, repeat.counter) {
        return Err(Error::Refused(
            "portable key counter did not advance".into(),
        ));
    }
    Ok((
        Protector {
            role,
            credential: enrolled.credential,
            key: enrolled.key,
            salt: enrolled.salt,
            secret: enrolled.secret,
        },
        repeat.counter,
    ))
}

/// An unlocked browsing session. Holds the vault key; never given to an app.
pub(super) struct Session {
    baseline: LockedVault,
    opened: OpenVault,
    // The directory the session was opened from; writes refuse any other.
    place: (u64, u64),
    credential: Vec<u8>,
    // Set by an uncertain publication; export is refused afterwards.
    uncertain: bool,
    // Verified counters observed in this session; not persisted.
    counters: Vec<(Vec<u8>, u32)>,
}

/// Enrolls a primary and a separately presented backup, proves both open
/// the identical empty notebook, then publishes revision one.
pub(super) fn create(
    directory: &Directory,
    tokens: &mut impl Tokens,
    random: &mut impl Read,
) -> Result<Session, Error> {
    let place = directory.place()?;
    let snapshot = directory.snapshot()?;
    if snapshot.vault().is_some() {
        return Err(Error::State("a portable vault already exists"));
    }
    let (primary, primary_counter) = enroll_proved(
        tokens,
        Purpose::CreatePrimary,
        Role::Primary,
        b"",
        &[],
        random,
    )?;
    let (backup, backup_counter) = enroll_proved(
        tokens,
        Purpose::CreateBackup,
        Role::Backup,
        b"",
        &[&primary.credential],
        random,
    )?;
    let empty = Notebook {
        entries: Vec::new(),
    };
    let protectors = [primary, backup];
    let proposed = LockedVault::create(&empty, &protectors, random).map_err(Error::Refused)?;
    // The backup was presented last and authorizes the opened session.
    let opened = prove_all(&proposed, &protectors, None)?;
    let counters = protectors
        .iter()
        .zip([primary_counter, backup_counter])
        .map(|(protector, counter)| (protector.credential.clone(), counter))
        .collect();
    let credential = protectors
        .last()
        .map(|protector| protector.credential.clone())
        .ok_or(Error::State("portable enrollment produced no key"))?;
    check_revoked(&*tokens)?;
    publish(directory, snapshot, &proposed, false, random)?;
    Ok(Session {
        baseline: proposed,
        opened,
        place,
        uncertain: false,
        credential,
        counters,
    })
}

/// Authenticates the whole proposal with the first protector, then requires
/// every other protector to unwrap the identical vault key, which must
/// differ from `old` when given.
fn prove_all(
    proposed: &LockedVault,
    protectors: &[Protector],
    old: Option<&OpenVault>,
) -> Result<OpenVault, Error> {
    let (first, rest) = protectors
        .split_first()
        .ok_or_else(|| Error::Refused("portable proposal has no keys".into()))?;
    let opened = proposed
        .open(&first.credential, &first.secret)
        .map_err(Error::Refused)?;
    if old.is_some_and(|old| old.same_key(&opened)) {
        return Err(Error::Refused(
            "portable rotation kept the old vault key".into(),
        ));
    }
    for protector in rest {
        if !proposed
            .unwraps_to(&protector.credential, &protector.secret, &opened)
            .map_err(Error::Refused)?
        {
            return Err(Error::Refused(
                "portable keys did not open one vault key".into(),
            ));
        }
    }
    Ok(opened)
}

/// Publishes against a snapshot taken before any token was presented.
fn publish(
    directory: &Directory,
    snapshot: Snapshot,
    proposed: &LockedVault,
    adopt: bool,
    random: &mut impl Read,
) -> Result<(), Error> {
    let store = directory.store()?;
    let result = if adopt {
        store.adopt(snapshot, proposed, random)
    } else {
        store.publish(snapshot, proposed, random)
    };
    result.map_err(|error| match error {
        PublishError::Rejected(error) => Error::Refused(error),
        PublishError::Uncertain(_) => Error::Uncertain,
    })
}

/// Authenticates one enrolled key against the committed vault for browsing.
pub(super) fn unlock(
    directory: &Directory,
    credential: &[u8],
    tokens: &mut impl Tokens,
    random: &mut impl Read,
) -> Result<Session, Error> {
    let place = directory.place()?;
    let snapshot = directory.snapshot()?;
    let baseline = LockedVault::decode(
        snapshot
            .vault()
            .ok_or(Error::State("no portable vault exists"))?
            .bytes(),
    )
    .map_err(Error::Refused)?;
    let hint = baseline
        .unlock_hint(credential)
        .map_err(|_| Error::State("that key is not enrolled in this vault"))?;
    let asserted = tokens.assert(
        Presented {
            purpose: Purpose::Unlock,
            role: hint.role,
            credential: Some(credential),
        },
        AssertRequest {
            credential,
            key: hint.key,
            salt: hint.salt,
            challenge: challenge(
                Purpose::Unlock,
                Phase::Assert,
                &binding(&baseline, b""),
                random,
            )?,
        },
    )?;
    let opened = baseline
        .open(credential, &asserted.secret)
        .map_err(Error::Refused)?;
    Ok(Session {
        baseline,
        opened,
        place,
        uncertain: false,
        credential: credential.to_vec(),
        counters: vec![(credential.to_vec(), asserted.counter)],
    })
}

/// Authenticates an exported copy with one of its keys, then places it,
/// identity and revision unchanged, into a location holding no vault.
pub(super) fn import(
    directory: &Directory,
    bytes: &[u8],
    credential: &[u8],
    tokens: &mut impl Tokens,
    random: &mut impl Read,
) -> Result<Session, Error> {
    let place = directory.place()?;
    let snapshot = directory.snapshot()?;
    if snapshot.vault().is_some() {
        return Err(Error::State("a portable vault already exists here"));
    }
    let baseline = LockedVault::decode(bytes).map_err(Error::Refused)?;
    let hint = baseline
        .unlock_hint(credential)
        .map_err(|_| Error::State("that key is not enrolled in this vault"))?;
    let asserted = tokens.assert(
        Presented {
            purpose: Purpose::Import,
            role: hint.role,
            credential: Some(credential),
        },
        AssertRequest {
            credential,
            key: hint.key,
            salt: hint.salt,
            challenge: challenge(
                Purpose::Import,
                Phase::Assert,
                &binding(&baseline, &crypto::digest(bytes)),
                random,
            )?,
        },
    )?;
    let opened = baseline
        .open(credential, &asserted.secret)
        .map_err(Error::Refused)?;
    check_revoked(&*tokens)?;
    publish(directory, snapshot, &baseline, true, random)?;
    Ok(Session {
        baseline,
        opened,
        place,
        uncertain: false,
        credential: credential.to_vec(),
        counters: vec![(credential.to_vec(), asserted.counter)],
    })
}

impl Session {
    pub fn notebook(&self) -> &Notebook {
        &self.opened.notebook
    }

    /// The session's authenticated baseline ciphertext, for export. Refused
    /// after an uncertain publication, when the store may already hold a
    /// newer revision than the session knows.
    pub fn ciphertext(&self) -> Result<&[u8], Error> {
        if self.uncertain {
            return Err(Error::State(
                "the last save's outcome is uncertain; lock and unlock again",
            ));
        }
        Ok(self.baseline.bytes())
    }

    pub fn revision(&self) -> u64 {
        self.baseline.revision()
    }

    pub fn keys(&self) -> Vec<KeyInfo> {
        self.baseline.unlock_hints().map(key_info).collect()
    }

    /// The enrolled key that authorizes this session's writes.
    pub fn credential(&self) -> &[u8] {
        &self.credential
    }

    pub fn use_key(&mut self, credential: &[u8]) -> Result<(), Error> {
        self.baseline
            .unlock_hint(credential)
            .map_err(|_| Error::State("that key is not enrolled in this vault"))?;
        self.credential = credential.to_vec();
        Ok(())
    }

    // Refuses a counter that regressed against this session's observations.
    fn advanced(&self, credential: &[u8], counter: u32) -> Result<(), Error> {
        match self.counters.iter().find(|(known, _)| known == credential) {
            Some((_, prior)) if !advanced(*prior, counter) => Err(Error::Refused(
                "portable key counter did not advance".into(),
            )),
            _ => Ok(()),
        }
    }

    // Applied only after the operation committed.
    fn record(&mut self, observed: Vec<(Vec<u8>, u32)>) {
        for (credential, counter) in observed {
            match self
                .counters
                .iter_mut()
                .find(|(known, _)| *known == credential)
            {
                Some((_, prior)) => *prior = counter,
                None => self.counters.push((credential, counter)),
            }
        }
    }

    /// A fresh assertion bound to this exact operation by an enrolled key,
    /// which must unwrap the session's own vault key.
    fn authorize(
        &self,
        purpose: Purpose,
        credential: &[u8],
        detail: &[u8],
        tokens: &mut impl Tokens,
        random: &mut impl Read,
    ) -> Result<(Protector, u32), Error> {
        let hint = self
            .baseline
            .unlock_hint(credential)
            .map_err(|_| Error::State("that key is not enrolled in this vault"))?;
        let (role, key, salt) = (hint.role, hint.key.clone(), *hint.salt);
        let challenge = challenge(
            purpose,
            Phase::Assert,
            &binding(&self.baseline, detail),
            random,
        )?;
        let asserted = tokens.assert(
            Presented {
                purpose,
                role,
                credential: Some(credential),
            },
            AssertRequest {
                credential,
                key: &key,
                salt: &salt,
                challenge,
            },
        )?;
        if !self
            .baseline
            .unwraps_to(credential, &asserted.secret, &self.opened)
            .map_err(Error::Refused)?
        {
            return Err(Error::Refused(
                "portable key opened a different vault key".into(),
            ));
        }
        self.advanced(credential, asserted.counter)?;
        Ok((
            Protector {
                role,
                credential: credential.to_vec(),
                key,
                salt,
                secret: asserted.secret,
            },
            asserted.counter,
        ))
    }

    // Taken before any token: refuses another location and a vault another
    // writer changed, and the store's baseline check refuses one changed
    // during authorization.
    fn current(&self, directory: &Directory) -> Result<Snapshot, Error> {
        if directory.place()? != self.place {
            return Err(Error::State(
                "this session belongs to another vault location",
            ));
        }
        let snapshot = directory.snapshot()?;
        if snapshot.vault().map(LockedVault::bytes) != Some(self.baseline.bytes()) {
            return Err(Error::State(
                "the portable vault changed; lock and unlock again",
            ));
        }
        Ok(snapshot)
    }

    fn commit(
        &mut self,
        directory: &Directory,
        snapshot: Snapshot,
        proposed: LockedVault,
        opened: OpenVault,
        random: &mut impl Read,
    ) -> Result<(), Error> {
        let published = publish(directory, snapshot, &proposed, false, random);
        if published == Err(Error::Uncertain) {
            self.uncertain = true;
        }
        published?;
        self.baseline = proposed;
        self.opened = opened;
        Ok(())
    }

    /// Publishes `notebook` as the next revision after fresh authorization
    /// bound to the exact proposed ciphertext.
    pub fn save(
        &mut self,
        directory: &Directory,
        notebook: &Notebook,
        tokens: &mut impl Tokens,
        random: &mut impl Read,
    ) -> Result<(), Error> {
        let snapshot = self.current(directory)?;
        let proposed = self
            .baseline
            .revise(&self.opened, notebook, random)
            .map_err(Error::Refused)?;
        let credential = self.credential.clone();
        let detail = crypto::digest(proposed.bytes());
        let (key, counter) = self.authorize(Purpose::Save, &credential, &detail, tokens, random)?;
        let opened = prove_all(&proposed, std::slice::from_ref(&key), None)?;
        if !opened.same_key(&self.opened) {
            return Err(Error::Refused("portable revision changed its key".into()));
        }
        check_revoked(&*tokens)?;
        self.commit(directory, snapshot, proposed, opened, random)?;
        self.record(vec![(credential, counter)]);
        Ok(())
    }

    /// Adds a backup after authorization by the session key, then proves
    /// creation and an independent repeat on the new key.
    pub fn add_key(
        &mut self,
        directory: &Directory,
        tokens: &mut impl Tokens,
        random: &mut impl Read,
    ) -> Result<(), Error> {
        let snapshot = self.current(directory)?;
        if self.baseline.unlock_hints().len() >= MAX_SLOTS {
            return Err(Error::State("the portable vault already has eight keys"));
        }
        let credential = self.credential.clone();
        let (authorizing, authorizing_counter) =
            self.authorize(Purpose::AuthorizeAddKey, &credential, b"", tokens, random)?;
        let excluded: Vec<&[u8]> = self
            .baseline
            .unlock_hints()
            .map(|hint| hint.credential)
            .collect();
        let (added, counter) = enroll_proved(
            tokens,
            Purpose::AddKey,
            Role::Backup,
            &binding(&self.baseline, b""),
            &excluded,
            random,
        )?;
        let added_id = added.credential.clone();
        let proposed = self
            .baseline
            .add_protector(&self.opened, &added, random)
            .map_err(Error::Refused)?;
        let opened = prove_all(&proposed, &[authorizing, added], None)?;
        if !opened.same_key(&self.opened) {
            return Err(Error::Refused("adding a key changed the vault key".into()));
        }
        check_revoked(&*tokens)?;
        self.commit(directory, snapshot, proposed, opened, random)?;
        self.record(vec![(credential, authorizing_counter), (added_id, counter)]);
        Ok(())
    }

    /// Revokes every key in `revoked` together and enrolls one replacement,
    /// which becomes primary when the primary is revoked. Every retained key
    /// participates, the vault key rotates and the notebook is re-encrypted;
    /// no revoked key can open the result.
    pub fn replace_keys(
        &mut self,
        directory: &Directory,
        revoked: &[&[u8]],
        tokens: &mut impl Tokens,
        random: &mut impl Read,
    ) -> Result<(), Error> {
        let snapshot = self.current(directory)?;
        let keys = self.keys();
        let mut role = Role::Backup;
        for (index, credential) in revoked.iter().enumerate() {
            let key = keys
                .iter()
                .find(|key| key.credential == *credential)
                .ok_or(Error::State("that key is not enrolled in this vault"))?;
            if revoked
                .get(..index)
                .is_some_and(|earlier| earlier.contains(credential))
            {
                return Err(Error::State("a revoked key is named twice"));
            }
            if key.role == Role::Primary {
                role = Role::Primary;
            }
        }
        let retained: Vec<&KeyInfo> = keys
            .iter()
            .filter(|key| !revoked.contains(&key.credential.as_slice()))
            .collect();
        if revoked.is_empty() || retained.is_empty() {
            return Err(Error::State(
                "revoke at least one key and retain at least one",
            ));
        }
        let detail = revocation(revoked);
        let mut protectors = Vec::new();
        let mut observed = Vec::new();
        for key in retained {
            let (protector, counter) = self.authorize(
                Purpose::AuthorizeReplaceKey,
                &key.credential,
                &detail,
                tokens,
                random,
            )?;
            observed.push((key.credential.clone(), counter));
            protectors.push(protector);
        }
        let excluded: Vec<&[u8]> = keys.iter().map(|key| key.credential.as_slice()).collect();
        let (replacement, counter) = enroll_proved(
            tokens,
            Purpose::ReplaceKey,
            role,
            &binding(&self.baseline, &detail),
            &excluded,
            random,
        )?;
        let replacement_id = replacement.credential.clone();
        observed.push((replacement_id.clone(), counter));
        let credential = if revoked.contains(&self.credential.as_slice()) {
            replacement_id
        } else {
            self.credential.clone()
        };
        protectors.push(replacement);
        let proposed = self
            .baseline
            .rotate(&self.opened, &protectors, random)
            .map_err(Error::Refused)?;
        let opened = prove_all(&proposed, &protectors, Some(&self.opened))?;
        if revoked
            .iter()
            .any(|credential| proposed.unlock_hint(credential).is_ok())
        {
            return Err(Error::Refused(
                "revoked portable key remains enrolled".into(),
            ));
        }
        check_revoked(&*tokens)?;
        self.commit(directory, snapshot, proposed, opened, random)?;
        self.credential = credential;
        self.counters
            .retain(|(known, _)| !revoked.contains(&known.as_slice()));
        self.record(observed);
        Ok(())
    }
}

// Order-independent digest of the revoked credential set.
fn revocation(revoked: &[&[u8]]) -> [u8; 32] {
    let mut sorted = revoked.to_vec();
    sorted.sort();
    let mut input = Vec::new();
    for credential in sorted {
        input.extend_from_slice(&(credential.len() as u64).to_be_bytes());
        input.extend_from_slice(credential);
    }
    crypto::digest(&input)
}

#[cfg(test)]
pub(in crate::portable) mod tests {
    use super::*;
    use crate::fido_p256::SecretScalar;
    use crate::fido_transaction::Status;
    use crate::portable::Entry;
    use std::collections::VecDeque;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(in crate::portable) struct Random(pub(in crate::portable) u64);
    impl Read for Random {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            for chunk in out.chunks_mut(32) {
                self.0 += 1;
                let block = crypto::digest(&self.0.to_be_bytes());
                chunk.copy_from_slice(&block[..chunk.len()]);
            }
            Ok(out.len())
        }
    }

    pub(in crate::portable) struct Place(
        pub(in crate::portable) PathBuf,
        pub(in crate::portable) Directory,
    );
    impl Place {
        pub(in crate::portable) fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "td-portable-lifecycle-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            let directory = Directory {
                file: File::open(&path).unwrap(),
                owner: fs::metadata(&path).unwrap().uid(),
            };
            Self(path, directory)
        }
        pub(in crate::portable) fn bytes(&self) -> Option<Vec<u8>> {
            fs::read(self.0.join("vault")).ok()
        }
    }
    impl Drop for Place {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Credential {
        id: Vec<u8>,
        key: VerificationKey,
        seed: [u8; 32],
    }
    struct Token {
        name: u8,
        credentials: Vec<Credential>,
        counter: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(in crate::portable) enum Call {
        Enroll(Purpose),
        Assert(Purpose),
    }

    /// Physical tokens; each call takes the next token the operator presents.
    #[derive(Default)]
    pub(in crate::portable) struct Bench {
        tokens: Vec<Token>,
        pub(in crate::portable) present: VecDeque<usize>,
        // Runs while a token is presented, as another writer might.
        during: Option<Box<dyn FnMut()>>,
        pub(in crate::portable) calls: Vec<Call>,
        challenges: Vec<[u8; 32]>,
        excluded: Vec<Vec<Vec<u8>>>,
        pub(in crate::portable) fail: Option<(usize, TokenError)>,
        corrupt: Option<usize>,
        frozen: bool,
        presented: Vec<(Role, Option<Vec<u8>>)>,
        // Set as a lock would, after the token exchange.
        pub(in crate::portable) revoked: bool,
    }

    fn cose(seed: &[u8; 32]) -> VerificationKey {
        let scalar = SecretScalar::from_bytes(Box::new(crypto::digest(seed))).unwrap();
        let (x, y) = scalar.public_key().unwrap().coordinates();
        let mut cose = vec![0xa5, 1, 2, 3, 0x26, 0x20, 1, 0x21, 0x58, 0x20];
        cose.extend(x);
        cose.extend([0x22, 0x58, 0x20]);
        cose.extend(y);
        VerificationKey::from_cose(&cose).unwrap()
    }

    impl Bench {
        pub(in crate::portable) fn new(count: u8) -> Self {
            Self {
                tokens: (0..count)
                    .map(|name| Token {
                        name,
                        credentials: Vec::new(),
                        counter: 0,
                    })
                    .collect(),
                ..Self::default()
            }
        }
        pub(in crate::portable) fn present(&mut self, order: &[usize]) -> &mut Self {
            self.present.extend(order);
            self
        }
        fn next(&mut self, call: Call) -> Result<usize, TokenError> {
            let index = self.calls.len();
            self.calls.push(call);
            let token = self.present.pop_front().expect("unexpected token request");
            match self.fail.take() {
                Some((at, error)) if at == index => Err(error),
                other => {
                    self.fail = other;
                    Ok(token)
                }
            }
        }
        fn secret(&self, index: usize, seed: &[u8; 32], salt: &[u8; 32]) -> Secret32 {
            let mut secret = crypto::hmac(seed, salt);
            if self.corrupt == Some(index) {
                secret[0] ^= 1;
            }
            Secret32(secret)
        }
        fn tick(&mut self, token: usize) -> u32 {
            let token = &mut self.tokens[token];
            if !self.frozen || token.counter == 0 {
                token.counter += 1;
            }
            token.counter
        }
        fn credential(&self, credential: &[u8]) -> usize {
            self.tokens
                .iter()
                .position(|t| t.credentials.iter().any(|c| c.id == credential))
                .unwrap()
        }
    }

    impl Tokens for Bench {
        fn enroll(
            &mut self,
            presented: Presented<'_>,
            request: EnrollRequest<'_>,
        ) -> Result<Enrolled, TokenError> {
            let index = self.calls.len();
            let token = self.next(Call::Enroll(presented.purpose))?;
            self.presented.push((presented.role, None));
            assert_eq!(presented.credential, None);
            self.challenges
                .extend([request.challenge, request.proof_challenge]);
            self.excluded
                .push(request.excluded.iter().map(|id| id.to_vec()).collect());
            if self.tokens[token]
                .credentials
                .iter()
                .any(|c| request.excluded.contains(&c.id.as_slice()))
            {
                return Err(TokenError::Failed(transaction::Error::Status(
                    Status::CredentialExcluded,
                )));
            }
            let name = self.tokens[token].name;
            let serial = self.tokens[token].credentials.len() as u8;
            let seed = crypto::digest(&[name, serial]);
            let credential = Credential {
                id: format!("token-{name}-credential-{serial}").into_bytes(),
                key: cose(&seed),
                seed,
            };
            let enrolled = Enrolled {
                credential: credential.id.clone(),
                key: credential.key.clone(),
                salt: request.salt,
                secret: self.secret(index, &seed, &request.salt),
                counter: self.tick(token),
            };
            self.tokens[token].credentials.push(credential);
            Ok(enrolled)
        }

        fn assert(
            &mut self,
            presented: Presented<'_>,
            request: AssertRequest<'_>,
        ) -> Result<Asserted, TokenError> {
            let index = self.calls.len();
            let token = self.next(Call::Assert(presented.purpose))?;
            if let Some(during) = self.during.as_mut() {
                during();
            }
            self.presented
                .push((presented.role, presented.credential.map(<[u8]>::to_vec)));
            assert_eq!(presented.credential, Some(request.credential));
            self.challenges.push(request.challenge);
            let Some(credential) = self.tokens[token]
                .credentials
                .iter()
                .find(|c| c.id == request.credential)
            else {
                return Err(TokenError::Failed(transaction::Error::Status(
                    Status::NoCredential,
                )));
            };
            if credential.key.cose() != request.key.cose() {
                return Err(TokenError::Failed(transaction::Error::Protocol(
                    "signature".into(),
                )));
            }
            let seed = credential.seed;
            Ok(Asserted {
                secret: self.secret(index, &seed, request.salt),
                counter: self.tick(token),
            })
        }

        fn revoked(&self) -> bool {
            self.revoked
        }
    }

    pub(in crate::portable) fn id(bench: &Bench, token: usize) -> Vec<u8> {
        bench.tokens[token].credentials[0].id.clone()
    }

    pub(in crate::portable) fn created(
        place: &Place,
        bench: &mut Bench,
        random: &mut Random,
    ) -> Session {
        bench.present(&[0, 0, 1, 1]);
        create(&place.1, bench, random).unwrap()
    }

    fn entry(title: &str, body: &[u8]) -> Notebook {
        Notebook {
            entries: vec![Entry::new([7; 16], 1, title.into(), body.to_vec())],
        }
    }

    pub(in crate::portable) fn unlocked(place: &Place, bench: &mut Bench, token: usize) -> Session {
        let credential = id(bench, token);
        bench.present(&[token]);
        unlock(&place.1, &credential, bench, &mut Random(9000)).unwrap()
    }

    #[test]
    fn creation_enrolls_two_separate_keys_that_each_unlock() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let session = created(&place, &mut bench, &mut Random(0));
        assert_eq!(session.revision(), 1);
        assert!(session.notebook().entries.is_empty());
        assert_eq!(session.credential(), id(&bench, 1));
        assert_eq!(
            bench.calls,
            [
                Call::Enroll(Purpose::CreatePrimary),
                Call::Assert(Purpose::CreatePrimary),
                Call::Enroll(Purpose::CreateBackup),
                Call::Assert(Purpose::CreateBackup),
            ]
        );
        assert_eq!(bench.excluded, [vec![], vec![id(&bench, 0)]]);
        let mut unique = bench.challenges.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 6);
        let keys = session.keys();
        assert!(keys
            .iter()
            .any(|k| k.role == Role::Primary && k.credential == id(&bench, 0)));
        assert!(keys
            .iter()
            .any(|k| k.role == Role::Backup && k.credential == id(&bench, 1)));
        drop(session);
        for token in [0, 1] {
            let session = unlocked(&place, &mut bench, token);
            assert!(session.notebook().entries.is_empty());
        }
        let primary = id(&bench, 0);
        bench.present(&[1]);
        assert_eq!(
            unlock(&place.1, &primary, &mut bench, &mut Random(1)).err(),
            Some(Error::Token(TokenError::Failed(
                transaction::Error::Status(Status::NoCredential)
            )))
        );
        assert!(bench.present.is_empty());
    }

    #[test]
    fn creation_publishes_nothing_after_any_refusal() {
        type Case = (&'static [usize], fn(&mut Bench));
        let cases: [Case; 6] = [
            (&[0, 0, 1], |b| b.fail = Some((2, TokenError::Unavailable))),
            // The operator presents the primary again as the backup.
            (&[0, 0, 0], |_| {}),
            (&[0, 0], |b| b.corrupt = Some(1)),
            (&[0, 0, 1, 1], |b| b.corrupt = Some(3)),
            (&[0, 0], |b| b.frozen = true),
            (&[0, 0, 1, 1], |b| {
                b.fail = Some((
                    3,
                    TokenError::Failed(transaction::Error::Status(Status::PinInvalid)),
                ))
            }),
        ];
        for (order, setup) in cases {
            let place = Place::new();
            let mut bench = Bench::new(2);
            bench.present(order);
            setup(&mut bench);
            assert!(create(&place.1, &mut bench, &mut Random(0)).is_err());
            assert!(bench.present.is_empty(), "retried after {order:?}");
            assert_eq!(place.bytes(), None);
            assert_eq!(
                keys(&place.1).err(),
                Some(Error::State("no portable vault exists"))
            );
        }
    }

    #[test]
    fn creation_refuses_an_existing_vault_before_any_token() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        drop(created(&place, &mut bench, &mut Random(0)));
        let calls = bench.calls.len();
        assert_eq!(
            create(&place.1, &mut bench, &mut Random(1)).err(),
            Some(Error::State("a portable vault already exists"))
        );
        assert_eq!(bench.calls.len(), calls);
    }

    #[test]
    fn saves_publish_fresh_bound_revisions_that_every_key_opens() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        for (revision, body) in [(2, b"first".as_slice()), (3, b"second")] {
            bench.present(&[1]);
            session
                .save(&place.1, &entry("Bank", body), &mut bench, &mut random)
                .unwrap();
            assert_eq!(session.revision(), revision);
            assert_eq!(session.notebook().entries[0].body(), body);
            assert_eq!(bench.calls.last(), Some(&Call::Assert(Purpose::Save)));
        }
        let saves = &bench.challenges[bench.challenges.len() - 2..];
        assert_ne!(saves[0], saves[1]);
        drop(session);
        for token in [0, 1] {
            let session = unlocked(&place, &mut bench, token);
            assert_eq!(session.revision(), 3);
            assert_eq!(session.notebook().entries[0].title(), "Bank");
            assert_eq!(session.notebook().entries[0].body(), b"second");
        }
    }

    #[test]
    fn a_revocation_after_the_last_token_exchange_publishes_nothing() {
        let cancelled = || {
            Error::Token(TokenError::Failed(transaction::Error::Interrupted(
                crate::fido_device::Interruption::Cancelled,
            )))
        };
        let place = Place::new();
        let mut bench = Bench::new(3);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        let stored = place.bytes().unwrap();
        bench.present(&[1]);
        bench.revoked = true;
        let refused = session.save(&place.1, &entry("Bank", b"late"), &mut bench, &mut random);
        assert_eq!(refused, Err(cancelled()));
        // The token was asked; only publication was stopped.
        assert_eq!(bench.calls.last(), Some(&Call::Assert(Purpose::Save)));
        bench.present(&[1, 2, 2]);
        assert_eq!(
            session.add_key(&place.1, &mut bench, &mut random),
            Err(cancelled())
        );
        assert_eq!(bench.calls.last(), Some(&Call::Assert(Purpose::AddKey)));
        let lost = id(&bench, 0);
        bench.present(&[1, 2, 2]);
        assert_eq!(
            session
                .replace_keys(&place.1, &[&lost], &mut bench, &mut random)
                .err(),
            Some(cancelled())
        );
        assert_eq!(bench.calls.last(), Some(&Call::Assert(Purpose::ReplaceKey)));
        assert_eq!(place.bytes(), Some(stored));
        // The session is unchanged and still saves once nothing revokes.
        bench.revoked = false;
        assert_eq!(session.revision(), 1);
        bench.present(&[1]);
        session
            .save(&place.1, &entry("Bank", b"kept"), &mut bench, &mut random)
            .unwrap();
        assert_eq!(unlocked(&place, &mut bench, 0).revision(), 2);
        let exported = session.ciphertext().unwrap().to_vec();
        drop(session);

        let fresh = Place::new();
        let backup = id(&bench, 1);
        bench.present(&[1]);
        bench.revoked = true;
        assert_eq!(
            import(&fresh.1, &exported, &backup, &mut bench, &mut random).err(),
            Some(cancelled())
        );
        assert_eq!(bench.calls.last(), Some(&Call::Assert(Purpose::Import)));
        assert_eq!(fresh.bytes(), None);

        let fresh = Place::new();
        let mut bench = Bench::new(2);
        bench.present(&[0, 0, 1, 1]);
        bench.revoked = true;
        assert_eq!(
            create(&fresh.1, &mut bench, &mut random).err(),
            Some(cancelled())
        );
        assert!(!exists(&fresh.1).unwrap());
    }

    #[test]
    fn refused_save_authorization_preserves_the_vault_and_session() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        let before = place.bytes();
        for (order, failure) in [
            (
                [1],
                Some(TokenError::Failed(transaction::Error::Status(
                    Status::PinInvalid,
                ))),
            ),
            ([1], Some(TokenError::Unavailable)),
            ([0], None),
        ] {
            bench.present(&order);
            if let Some(failure) = failure {
                bench.fail = Some((bench.calls.len(), failure));
            }
            assert!(matches!(
                session.save(&place.1, &entry("A", b"x"), &mut bench, &mut random),
                Err(Error::Token(_))
            ));
            assert_eq!(place.bytes(), before);
            assert_eq!(session.revision(), 1);
        }
        bench.present(&[1]);
        session
            .save(&place.1, &entry("A", b"x"), &mut bench, &mut random)
            .unwrap();
        assert_eq!(session.revision(), 2);
    }

    #[test]
    fn writes_from_a_stale_session_are_refused_before_any_token() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let mut random = Random(0);
        drop(created(&place, &mut bench, &mut random));
        let mut first = unlocked(&place, &mut bench, 0);
        let mut second = unlocked(&place, &mut bench, 1);
        bench.present(&[0]);
        first
            .save(&place.1, &entry("A", b"first"), &mut bench, &mut random)
            .unwrap();
        let committed = place.bytes();
        let calls = bench.calls.len();
        let primary = id(&bench, 0);
        let stale = Some(Error::State(
            "the portable vault changed; lock and unlock again",
        ));
        for body in [b"second".as_slice(), b"again"] {
            let saved = second.save(&place.1, &entry("B", body), &mut bench, &mut random);
            assert_eq!(saved.err(), stale);
        }
        assert_eq!(
            second.add_key(&place.1, &mut bench, &mut random).err(),
            stale
        );
        let replaced = second.replace_keys(&place.1, &[&primary], &mut bench, &mut random);
        assert_eq!(replaced.err(), stale);
        assert_eq!(bench.calls.len(), calls);
        assert_eq!(place.bytes(), committed);
    }

    #[test]
    fn a_change_during_authorization_is_refused_at_publication() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        let path = place.0.clone();
        // Another writer replaces the file with identical bytes, a new inode.
        bench.during = Some(Box::new(move || {
            let copy = path.join("copy");
            fs::copy(path.join("vault"), &copy).unwrap();
            fs::rename(copy, path.join("vault")).unwrap();
        }));
        bench.present(&[1]);
        assert!(matches!(
            session.save(&place.1, &entry("A", b"x"), &mut bench, &mut random),
            Err(Error::Refused(_))
        ));
        bench.during = None;
        assert_eq!(session.revision(), 1);
        // The bytes are unchanged, so the next write proceeds.
        bench.present(&[1]);
        session
            .save(&place.1, &entry("A", b"x"), &mut bench, &mut random)
            .unwrap();
        assert_eq!(session.revision(), 2);
    }

    #[test]
    fn writes_refuse_another_location_holding_the_same_vault() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        let copy = Place::new();
        fs::copy(place.0.join("vault"), copy.0.join("vault")).unwrap();
        let calls = bench.calls.len();
        let elsewhere = Some(Error::State(
            "this session belongs to another vault location",
        ));
        let primary = id(&bench, 0);
        let saved = session.save(&copy.1, &entry("A", b"x"), &mut bench, &mut random);
        assert_eq!(saved.err(), elsewhere);
        assert_eq!(
            session.add_key(&copy.1, &mut bench, &mut random).err(),
            elsewhere
        );
        let replaced = session.replace_keys(&copy.1, &[&primary], &mut bench, &mut random);
        assert_eq!(replaced.err(), elsewhere);
        // Refused before the revocation list is read.
        let replaced = session.replace_keys(&copy.1, &[], &mut bench, &mut random);
        assert_eq!(replaced.err(), elsewhere);
        assert_eq!(bench.calls.len(), calls);
        assert_eq!(copy.bytes(), place.bytes());
    }

    #[test]
    fn added_keys_keep_the_vault_key_and_every_key_opens() {
        let place = Place::new();
        let mut bench = Bench::new(3);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        bench.present(&[1]);
        session
            .save(&place.1, &entry("Kept", b"body"), &mut bench, &mut random)
            .unwrap();
        let old = LockedVault::decode(&place.bytes().unwrap()).unwrap();
        bench.present(&[1, 2, 2]);
        session.add_key(&place.1, &mut bench, &mut random).unwrap();
        assert_eq!(
            bench.calls[bench.calls.len() - 3..],
            [
                Call::Assert(Purpose::AuthorizeAddKey),
                Call::Enroll(Purpose::AddKey),
                Call::Assert(Purpose::AddKey),
            ]
        );
        assert_eq!(
            bench.excluded.last().unwrap(),
            &old.unlock_hints()
                .map(|h| h.credential.to_vec())
                .collect::<Vec<_>>()
        );
        assert_eq!(session.keys().len(), 3);
        drop(session);
        let added = unlocked(&place, &mut bench, 2);
        let original = unlocked(&place, &mut bench, 0);
        assert!(added.opened.same_key(&original.opened));
        assert_eq!(added.notebook().entries[0].body(), b"body");
        let seed = bench.tokens[0].credentials[0].seed;
        let salt = *old.unlock_hint(&id(&bench, 0)).unwrap().salt;
        let previous = old
            .open(&id(&bench, 0), &Secret32(crypto::hmac(&seed, &salt)))
            .unwrap();
        assert!(previous.same_key(&added.opened));
    }

    #[test]
    fn refused_key_additions_publish_nothing() {
        let place = Place::new();
        let mut bench = Bench::new(3);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        let before = place.bytes();
        // An already enrolled token is refused by its exclusion list.
        bench.present(&[1, 0]);
        assert_eq!(
            session.add_key(&place.1, &mut bench, &mut random).err(),
            Some(Error::Token(TokenError::Failed(
                transaction::Error::Status(Status::CredentialExcluded)
            )))
        );
        bench.present(&[1, 2, 2]);
        bench.corrupt = Some(bench.calls.len() + 2);
        assert!(matches!(
            session.add_key(&place.1, &mut bench, &mut random),
            Err(Error::Refused(_))
        ));
        assert!(bench.present.is_empty());
        assert_eq!(place.bytes(), before);
        assert_eq!(session.keys().len(), 2);
    }

    #[test]
    fn a_ninth_key_is_refused_before_any_token() {
        let place = Place::new();
        let mut bench = Bench::new(8);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        for token in 2..8 {
            bench.present(&[1, token, token]);
            session.add_key(&place.1, &mut bench, &mut random).unwrap();
        }
        assert_eq!(session.keys().len(), 8);
        let calls = bench.calls.len();
        assert_eq!(
            session.add_key(&place.1, &mut bench, &mut random).err(),
            Some(Error::State("the portable vault already has eight keys"))
        );
        assert_eq!(bench.calls.len(), calls);
    }

    #[test]
    fn replacing_a_lost_primary_rotates_and_revokes_it() {
        let place = Place::new();
        let mut bench = Bench::new(3);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        bench.present(&[1]);
        session
            .save(&place.1, &entry("Kept", b"body"), &mut bench, &mut random)
            .unwrap();
        let old_bytes = place.bytes().unwrap();
        let lost = id(&bench, 0);
        bench.present(&[1, 2, 2]);
        session
            .replace_keys(&place.1, &[&lost], &mut bench, &mut random)
            .unwrap();
        assert_eq!(
            bench.calls[bench.calls.len() - 3..],
            [
                Call::Assert(Purpose::AuthorizeReplaceKey),
                Call::Enroll(Purpose::ReplaceKey),
                Call::Assert(Purpose::ReplaceKey),
            ]
        );
        assert_eq!(bench.excluded.last().unwrap().len(), 2);
        assert_eq!(session.credential(), id(&bench, 1));
        let keys = session.keys();
        assert!(keys
            .iter()
            .any(|k| k.role == Role::Primary && k.credential == id(&bench, 2)));
        assert!(!keys.iter().any(|k| k.credential == lost));
        drop(session);
        assert_eq!(
            unlock(&place.1, &lost, &mut bench, &mut Random(1)).err(),
            Some(Error::State("that key is not enrolled in this vault"))
        );
        let replacement = unlocked(&place, &mut bench, 2);
        assert_eq!(replacement.notebook().entries[0].body(), b"body");
        assert_eq!(replacement.revision(), 3);
        // The historical copy still opens with the lost key, under the old key.
        let old = LockedVault::decode(&old_bytes).unwrap();
        let seed = bench.tokens[0].credentials[0].seed;
        let salt = *old.unlock_hint(&lost).unwrap().salt;
        let historical = old
            .open(&lost, &Secret32(crypto::hmac(&seed, &salt)))
            .unwrap();
        assert!(!historical.same_key(&replacement.opened));
    }

    #[test]
    fn replacement_requires_every_retained_key_and_moves_a_revoked_session() {
        let place = Place::new();
        let mut bench = Bench::new(4);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        bench.present(&[1, 2, 2]);
        session.add_key(&place.1, &mut bench, &mut random).unwrap();
        let before = place.bytes();
        let revoked = id(&bench, 1);
        // The retained primary is presented, but the retained backup is not.
        bench.present(&[0, 1]);
        assert!(session
            .replace_keys(&place.1, &[&revoked], &mut bench, &mut random)
            .is_err());
        assert_eq!(place.bytes(), before);
        bench.present(&[0, 2, 3, 3]);
        session
            .replace_keys(&place.1, &[&revoked], &mut bench, &mut random)
            .unwrap();
        assert_eq!(
            bench.calls[bench.calls.len() - 4..],
            [
                Call::Assert(Purpose::AuthorizeReplaceKey),
                Call::Assert(Purpose::AuthorizeReplaceKey),
                Call::Enroll(Purpose::ReplaceKey),
                Call::Assert(Purpose::ReplaceKey),
            ]
        );
        let replacement = bench.tokens[3].credentials[0].id.clone();
        assert_eq!(session.credential(), replacement);
        assert!(session
            .keys()
            .iter()
            .any(|k| k.role == Role::Backup && k.credential == replacement));
        bench.present(&[3]);
        session
            .save(&place.1, &entry("After", b"x"), &mut bench, &mut random)
            .unwrap();
        assert_eq!(bench.credential(&replacement), 3);
    }

    #[test]
    fn operation_challenges_bind_purpose_phase_and_operation() {
        let base = challenge(Purpose::Save, Phase::Assert, b"one", &mut Random(5)).unwrap();
        for other in [
            challenge(Purpose::Unlock, Phase::Assert, b"one", &mut Random(5)),
            challenge(Purpose::Save, Phase::Repeat, b"one", &mut Random(5)),
            challenge(Purpose::Save, Phase::Assert, b"two", &mut Random(5)),
            challenge(Purpose::Save, Phase::Assert, b"one", &mut Random(6)),
        ] {
            assert_ne!(other.unwrap(), base);
        }
        assert_eq!(
            challenge(Purpose::Save, Phase::Assert, b"one", &mut Random(5)).unwrap(),
            base
        );
        assert!(challenge(Purpose::Save, Phase::Assert, b"", &mut std::io::empty()).is_err());
    }

    #[test]
    fn creation_names_the_role_and_key_of_each_presentation() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        drop(created(&place, &mut bench, &mut Random(0)));
        assert_eq!(
            bench.presented,
            [
                (Role::Primary, None),
                (Role::Primary, Some(id(&bench, 0))),
                (Role::Backup, None),
                (Role::Backup, Some(id(&bench, 1))),
            ]
        );
    }

    #[test]
    fn a_stolen_key_and_its_additions_are_revoked_together() {
        let place = Place::new();
        let mut bench = Bench::new(4);
        let mut random = Random(0);
        // The attacker unlocks with the stolen backup and adds a key.
        let mut attacker = created(&place, &mut bench, &mut random);
        bench.present(&[1, 2, 2]);
        attacker.add_key(&place.1, &mut bench, &mut random).unwrap();
        drop(attacker);
        let (stolen, planted) = (id(&bench, 1), id(&bench, 2));
        let mut owner = unlocked(&place, &mut bench, 0);
        bench.present(&[0, 3, 3]);
        owner
            .replace_keys(&place.1, &[&stolen, &planted], &mut bench, &mut random)
            .unwrap();
        assert_eq!(
            bench.calls[bench.calls.len() - 3..],
            [
                Call::Assert(Purpose::AuthorizeReplaceKey),
                Call::Enroll(Purpose::ReplaceKey),
                Call::Assert(Purpose::ReplaceKey),
            ]
        );
        let keys = owner.keys();
        assert_eq!(keys.len(), 2);
        assert!(keys
            .iter()
            .any(|k| k.role == Role::Primary && k.credential == id(&bench, 0)));
        assert!(keys
            .iter()
            .any(|k| k.role == Role::Backup && k.credential == id(&bench, 3)));
        drop(owner);
        for revoked in [stolen, planted] {
            assert_eq!(
                unlock(&place.1, &revoked, &mut bench, &mut Random(1)).err(),
                Some(Error::State("that key is not enrolled in this vault"))
            );
        }
    }

    #[test]
    fn revocation_sets_are_checked_before_any_token() {
        let place = Place::new();
        let mut bench = Bench::new(3);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        bench.present(&[1, 2, 2]);
        session.add_key(&place.1, &mut bench, &mut random).unwrap();
        let (a, b, c) = (id(&bench, 0), id(&bench, 1), id(&bench, 2));
        let calls = bench.calls.len();
        let before = place.bytes();
        let retain = "revoke at least one key and retain at least one";
        for (set, refused) in [
            (vec![], retain),
            (vec![a.as_slice(), &b, &c], retain),
            (vec![a.as_slice(), &a], "a revoked key is named twice"),
            (
                vec![b"unknown".as_slice()],
                "that key is not enrolled in this vault",
            ),
        ] {
            assert_eq!(
                session
                    .replace_keys(&place.1, &set, &mut bench, &mut random)
                    .err(),
                Some(Error::State(refused))
            );
        }
        assert_eq!(bench.calls.len(), calls);
        assert_eq!(place.bytes(), before);
    }

    #[test]
    fn refused_replacement_enrollment_publishes_nothing() {
        let place = Place::new();
        let mut bench = Bench::new(3);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        let before = place.bytes();
        let lost = id(&bench, 0);
        // The retained backup is offered again as the replacement.
        bench.present(&[1, 1]);
        assert_eq!(
            session
                .replace_keys(&place.1, &[&lost], &mut bench, &mut random)
                .err(),
            Some(Error::Token(TokenError::Failed(
                transaction::Error::Status(Status::CredentialExcluded)
            )))
        );
        bench.present(&[1, 2, 2]);
        bench.corrupt = Some(bench.calls.len() + 2);
        assert!(matches!(
            session.replace_keys(&place.1, &[&lost], &mut bench, &mut random),
            Err(Error::Refused(_))
        ));
        assert!(bench.present.is_empty());
        assert_eq!(place.bytes(), before);
        assert!(session.keys().iter().any(|k| k.credential == lost));
    }

    #[test]
    fn a_counter_that_did_not_advance_refuses_the_write() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        let before = place.bytes();
        bench.frozen = true;
        bench.present(&[1]);
        assert_eq!(
            session
                .save(&place.1, &entry("A", b"x"), &mut bench, &mut random)
                .err(),
            Some(Error::Refused(
                "portable key counter did not advance".into()
            ))
        );
        assert_eq!(place.bytes(), before);
        assert_eq!(session.revision(), 1);
    }

    #[test]
    fn a_busy_store_refuses_the_write_and_leaves_the_session_usable() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        let open =
            |place: &Place| Store::open(place.1.file.try_clone().unwrap(), place.1.owner).unwrap();
        // Busy before authorization: refused with no token presented.
        let held = open(&place);
        let calls = bench.calls.len();
        assert!(matches!(
            session.save(&place.1, &entry("A", b"x"), &mut bench, &mut random),
            Err(Error::Store(_))
        ));
        assert_eq!(bench.calls.len(), calls);
        drop(held);
        // Busy only once the token is presented: refused at publication.
        let held = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (slot, directory) = (held.clone(), place.1.file.try_clone().unwrap());
        let owner = place.1.owner;
        bench.during = Some(Box::new(move || {
            *slot.borrow_mut() = Some(Store::open(directory.try_clone().unwrap(), owner).unwrap());
        }));
        bench.present(&[1]);
        assert!(matches!(
            session.save(&place.1, &entry("A", b"x"), &mut bench, &mut random),
            Err(Error::Store(_))
        ));
        bench.during = None;
        held.borrow_mut().take();
        for revision in [2, 3] {
            bench.present(&[1]);
            session
                .save(&place.1, &entry("A", b"x"), &mut bench, &mut random)
                .unwrap();
            assert_eq!(session.revision(), revision);
        }
    }

    #[test]
    fn the_session_key_selects_which_token_authorizes_writes() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        assert_eq!(
            session.use_key(b"unknown").err(),
            Some(Error::State("that key is not enrolled in this vault"))
        );
        assert_eq!(session.credential(), id(&bench, 1));
        session.use_key(&id(&bench, 0)).unwrap();
        bench.present(&[0]);
        session
            .save(&place.1, &entry("A", b"x"), &mut bench, &mut random)
            .unwrap();
        assert_eq!(
            bench.presented.last(),
            Some(&(Role::Primary, Some(id(&bench, 0))))
        );
    }

    #[test]
    fn an_unavailable_token_leaves_the_vault_locked() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        drop(created(&place, &mut bench, &mut Random(0)));
        let primary = id(&bench, 0);
        bench.present(&[0]);
        bench.fail = Some((bench.calls.len(), TokenError::Unavailable));
        assert_eq!(
            unlock(&place.1, &primary, &mut bench, &mut Random(1)).err(),
            Some(Error::Token(TokenError::Unavailable))
        );
    }

    #[test]
    fn export_is_refused_after_an_uncertain_publication() {
        let place = Place::new();
        let mut bench = Bench::new(2);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        assert!(session.ciphertext().is_ok());
        session.uncertain = true;
        assert_eq!(
            session.ciphertext().err(),
            Some(Error::State(
                "the last save's outcome is uncertain; lock and unlock again",
            ))
        );
    }

    #[test]
    fn an_exported_copy_recovers_on_a_fresh_machine_with_only_the_backup() {
        let place = Place::new();
        let mut bench = Bench::new(3);
        let mut random = Random(0);
        let mut session = created(&place, &mut bench, &mut random);
        bench.present(&[1]);
        session
            .save(&place.1, &entry("Kept", b"body"), &mut bench, &mut random)
            .unwrap();
        let exported = session.ciphertext().unwrap().to_vec();
        drop(session);
        let fresh = Place::new();
        let backup = id(&bench, 1);
        let calls = bench.calls.len();
        assert_eq!(
            import(&fresh.1, &exported, b"unknown", &mut bench, &mut random).err(),
            Some(Error::State("that key is not enrolled in this vault"))
        );
        assert!(matches!(
            import(&fresh.1, &exported[..20], &backup, &mut bench, &mut random),
            Err(Error::Refused(_))
        ));
        assert_eq!(bench.calls.len(), calls);
        // A tampered body, and a tampered salt in the backup's own slot.
        let mut body = exported.clone();
        *body.last_mut().unwrap() ^= 1;
        let decoded = LockedVault::decode(&exported).unwrap();
        let salt = decoded.unlock_hint(&backup).unwrap().salt;
        let at = exported.windows(32).position(|w| w == salt).unwrap();
        let mut slot = exported.clone();
        slot[at] ^= 1;
        for tampered in [body, slot] {
            bench.present(&[1]);
            assert!(matches!(
                import(&fresh.1, &tampered, &backup, &mut bench, &mut random),
                Err(Error::Refused(_))
            ));
        }
        bench.present(&[0]);
        assert!(matches!(
            import(&fresh.1, &exported, &backup, &mut bench, &mut random),
            Err(Error::Token(_))
        ));
        assert_eq!(fresh.bytes(), None);
        bench.present(&[1]);
        let mut restored = import(&fresh.1, &exported, &backup, &mut bench, &mut random).unwrap();
        assert_eq!(bench.calls.last(), Some(&Call::Assert(Purpose::Import)));
        assert_eq!(fresh.bytes(), Some(exported.clone()));
        assert_eq!(restored.revision(), 2);
        assert_eq!(restored.notebook().entries[0].body(), b"body");
        // The primary is lost: the backup and a new token replace it.
        let lost = id(&bench, 0);
        bench.present(&[1, 2, 2]);
        restored
            .replace_keys(&fresh.1, &[&lost], &mut bench, &mut random)
            .unwrap();
        assert_eq!(restored.revision(), 3);
        let calls = bench.calls.len();
        assert_eq!(
            import(&fresh.1, &exported, &backup, &mut bench, &mut random).err(),
            Some(Error::State("a portable vault already exists here"))
        );
        assert_eq!(bench.calls.len(), calls);
    }

    mod hardware {
        use super::*;
        use crate::fido_transaction::tests::{assertion, enrollment, entropy, fixture, Script};

        const LABELS: [&str; 4] = ["p1-legacy", "p1-scoped", "p2-legacy", "p2-scoped"];

        #[test]
        fn the_adapter_returns_only_verified_transcript_results() {
            for label in LABELS {
                let (script, trace) = Script::new(label, true);
                let mut channel = Some(script);
                let mut purposes = Vec::new();
                let mut adapter = Hardware {
                    open: |_: Presented<'_>| channel.take().ok_or(TokenError::Unavailable),
                    prompt: |presented: Presented<'_>, pin| {
                        purposes.push((presented.purpose, presented.role, pin));
                        Pin::new(fixture(label, "pin").into_boxed_slice())
                    },
                    entropy: entropy(label, true),
                };
                let intent = enrollment(label);
                let enrolled = adapter
                    .enroll(
                        Presented {
                            purpose: Purpose::AddKey,
                            role: Role::Backup,
                            credential: None,
                        },
                        EnrollRequest {
                            challenge: intent.challenge,
                            proof_challenge: intent.proof_challenge,
                            user: intent.user,
                            salt: intent.salt,
                            excluded: &[],
                        },
                    )
                    .unwrap();
                drop(adapter);
                assert_eq!(enrolled.credential, fixture(label, "credential_id"));
                assert_eq!(enrolled.key.cose().as_slice(), fixture(label, "cose"));
                assert_eq!(enrolled.secret.0.as_slice(), fixture(label, "output"));
                assert_eq!(
                    purposes,
                    [
                        (Purpose::AddKey, Role::Backup, PinPurpose::Creation),
                        (Purpose::AddKey, Role::Backup, PinPurpose::EnrollmentProof)
                    ]
                );
                assert_eq!(trace.drops.get(), 1);

                let (script, trace) = Script::new(label, false);
                let mut channel = Some(script);
                let mut opened = Vec::new();
                let mut adapter = Hardware {
                    open: |presented: Presented<'_>| {
                        opened.push(presented.credential.map(<[u8]>::to_vec));
                        channel.take().ok_or(TokenError::Unavailable)
                    },
                    prompt: |_: Presented<'_>, _| {
                        Pin::new(fixture(label, "pin").into_boxed_slice())
                    },
                    entropy: entropy(label, false),
                };
                let intent = assertion(label);
                let (x, y) = intent.key.coordinates();
                let mut cose = vec![0xa5, 1, 2, 3, 0x26, 0x20, 1, 0x21, 0x58, 0x20];
                cose.extend(x);
                cose.extend([0x22, 0x58, 0x20]);
                cose.extend(y);
                let key = VerificationKey::from_cose(&cose).unwrap();
                let asserted = adapter
                    .assert(
                        Presented {
                            purpose: Purpose::Unlock,
                            role: Role::Primary,
                            credential: Some(intent.credential),
                        },
                        AssertRequest {
                            credential: intent.credential,
                            key: &key,
                            salt: &intent.salt,
                            challenge: intent.challenge,
                        },
                    )
                    .unwrap();
                assert_eq!(asserted.secret.0.as_slice(), fixture(label, "output"));
                drop(adapter);
                assert_eq!(opened, [Some(intent.credential.to_vec())]);
                assert_eq!(trace.drops.get(), 1);
            }
        }

        #[test]
        fn an_unopened_channel_keeps_its_reason_and_prompts_for_nothing() {
            let key = cose(&[1; 32]);
            let reasons: [fn() -> TokenError; 4] = [
                || TokenError::Unavailable,
                || TokenError::Denied,
                || TokenError::Several,
                || TokenError::Host("swap"),
            ];
            for reason in reasons {
                let mut prompted = false;
                let mut opened = 0;
                let mut adapter = Hardware {
                    open: |_: Presented<'_>| {
                        opened += 1;
                        Err::<Script, _>(reason())
                    },
                    prompt: |_: Presented<'_>, _| {
                        prompted = true;
                        Err("unexpected".to_string())
                    },
                    entropy: |_: &mut [u8]| Ok(()),
                };
                let enrolled = adapter.enroll(
                    Presented {
                        purpose: Purpose::CreatePrimary,
                        role: Role::Primary,
                        credential: None,
                    },
                    EnrollRequest {
                        challenge: [0; 32],
                        proof_challenge: [0; 32],
                        user: [0; 32],
                        salt: [0; 32],
                        excluded: &[],
                    },
                );
                assert_eq!(enrolled.err(), Some(reason()));
                let result = adapter.assert(
                    Presented {
                        purpose: Purpose::Unlock,
                        role: Role::Primary,
                        credential: Some(b"id"),
                    },
                    AssertRequest {
                        credential: b"id",
                        key: &key,
                        salt: &[0; 32],
                        challenge: [0; 32],
                    },
                );
                assert_eq!(result.err(), Some(reason()));
                assert!(!prompted);
                assert_eq!(opened, 2);
            }
        }
    }
}
