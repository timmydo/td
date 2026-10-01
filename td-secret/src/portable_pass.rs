//! The notebook API td-pass links: standalone mode on the invoking desktop
//! account, over the same lifecycle, notebook and host adapter as every
//! other caller. It hands out entries and typed failures, never a key, a
//! token's output or a PIN; the PIN prompt is the caller's `Prompt`, and
//! every token operation ends when its `Cancel` is cancelled.

use super::host::{self, Entropy, Protected};
use super::lifecycle::{
    self, AssertRequest, Asserted, Directory, EnrollRequest, Enrolled, Error, Hardware, KeyInfo,
    Presented, Session, TokenError, Tokens,
};
use super::notebook::{self, EntryError, Text};
use super::{LockedVault, Role};
use crate::fido_device::{self, Cancellation, Interruption};
use crate::fido_pin::Pin;
use crate::fido_transaction::{Error as Transaction, PinPurpose, Status};
use std::cell::{Cell, RefCell};
use std::io::{self, Read};

/// The notebook's bounds, for an editor to refuse early what a save would.
pub const MAX_TITLE: usize = super::MAX_TITLE;
pub const MAX_BODY: usize = super::MAX_BODY;
pub const MAX_ENTRIES: usize = super::MAX_ENTRIES;

pub type EntryId = [u8; 16];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRole {
    Primary,
    Backup,
}

impl From<Role> for KeyRole {
    fn from(role: Role) -> Self {
        match role {
            Role::Primary => Self::Primary,
            Role::Backup => Self::Backup,
        }
    }
}

/// A short public name for an enrolled credential, so a person can tell
/// two backups apart: the first four bytes of its SHA-256, in hex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint([u8; 4]);

impl Fingerprint {
    fn of(credential: &[u8]) -> Self {
        let [a, b, c, d, ..] = crate::crypto::digest(credential);
        Self([a, b, c, d])
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// One enrolled key: its role and the public credential identity that
/// names it to a token. It holds no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    role: KeyRole,
    credential: Vec<u8>,
}

impl Key {
    pub fn role(&self) -> KeyRole {
        self.role
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.credential)
    }
}

impl From<KeyInfo> for Key {
    fn from(info: KeyInfo) -> Self {
        Self {
            role: info.role.into(),
            credential: info.credential,
        }
    }
}

/// One token presentation: what it authorizes, in words that carry no
/// notebook content, which role, and for an enrolled key its fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub operation: &'static str,
    pub role: KeyRole,
    pub key: Option<Fingerprint>,
}

impl From<Presented<'_>> for Request {
    fn from(presented: Presented<'_>) -> Self {
        Self {
            operation: presented.purpose.label(),
            role: presented.role.into(),
            key: presented.credential.map(Fingerprint::of),
        }
    }
}

/// Why the presented key needs its PIN. `Request::operation` names what
/// an authorization is for: unlocking, a save, a key change or an import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinUse {
    /// An enrolled key authorizing the named operation.
    Authorize,
    Enroll,
    /// The second PIN of an enrollment, proving the new credential.
    Proof,
}

/// The host authentication adapter's prompt, shown by the caller. An
/// error from either method is the person declining: the operation ends
/// as cancelled, before the token is used further, and is not retried.
pub trait Prompt {
    /// Before each token is opened: the one key that should be connected
    /// now. Returning lets the operation open it.
    fn present(&mut self, request: Request) -> Result<(), String>;
    /// The PIN for the presented key, as 4 through 63 printable ASCII
    /// bytes; the buffer is zeroed when the operation drops it.
    fn pin(&mut self, request: Request, purpose: PinUse) -> Result<Box<[u8]>, String>;
}

/// Ends an operation from another thread, as a lock does: the token
/// session in flight, a presentation not yet asked for, a publication not
/// yet begun and an unlock not yet returned. Once cancelled it stays
/// cancelled, so each operation the caller may cancel takes a fresh one.
#[derive(Clone)]
pub struct Cancel(Cancellation);

impl Default for Cancel {
    fn default() -> Self {
        Self(Cancellation::new())
    }
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancel();
    }
}

fn cancelled() -> Error {
    Error::Token(TokenError::Failed(Transaction::Interrupted(
        Interruption::Cancelled,
    )))
}

/// A refused or failed operation. Its text, and its `Debug`, name the
/// reason without any PIN, entry content or token output.
#[derive(PartialEq, Eq)]
pub struct Failure(Error);

impl Failure {
    /// The entry changed since it was read; nothing was saved.
    pub fn stale(&self) -> bool {
        self.0 == Error::Entry(EntryError::Stale)
    }

    /// Publication may or may not have happened; lock and unlock again.
    pub fn uncertain(&self) -> bool {
        self.0 == Error::Uncertain
    }

    /// The caller's `Cancel` or its declining `Prompt` ended the operation.
    pub fn cancelled(&self) -> bool {
        self.0 == cancelled()
    }
}

impl std::fmt::Debug for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Failure").field(&self.to_string()).finish()
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Error::Token(token) => f.write_str(token_text(token)),
            Error::Store(reason) => write!(f, "the vault store refused: {reason}"),
            Error::State(reason) => f.write_str(reason),
            Error::Refused(reason) => f.write_str(reason),
            Error::Entry(entry) => f.write_str(match entry {
                EntryError::Missing => "the entry no longer exists",
                EntryError::Stale => "the entry changed since it was opened; nothing was saved",
                EntryError::Title => "a title is 1 to 512 bytes of text without control characters",
                EntryError::Duplicate => "another entry has this title",
                EntryError::Body => "the entry is too large",
                EntryError::Full => "the notebook is full",
            }),
            Error::Uncertain => f.write_str(
                "the save may or may not have been published; lock and unlock to see the vault",
            ),
        }
    }
}

impl std::error::Error for Failure {}

fn token_text(error: &TokenError) -> &'static str {
    match error {
        TokenError::Unavailable => "no security key is connected",
        TokenError::Denied => "the host's device policy denies this account the security key",
        TokenError::Several => "more than one security key is connected; connect only one",
        TokenError::Host(reason) => reason,
        TokenError::Failed(failed) => match failed {
            Transaction::Interrupted(Interruption::Cancelled) => "cancelled",
            Transaction::Interrupted(Interruption::Expired) => {
                "the security key did not finish in time"
            }
            Transaction::Interrupted(Interruption::Closed) => "the security key was disconnected",
            Transaction::Transport => "the security key's connection failed",
            Transaction::Protocol(_) => "the security key's answer was refused",
            Transaction::PinInput => "a PIN is 4 to 63 printable ASCII characters",
            Transaction::Entropy => "kernel entropy is unavailable",
            Transaction::Status(status) => match status {
                Status::PinInvalid => "wrong PIN",
                Status::PinBlocked | Status::PinAuthBlocked => {
                    "the security key's PIN is blocked; see the key's own recovery"
                }
                Status::PinAuthInvalid => "the security key refused the PIN exchange",
                Status::PinNotSet => "the security key has no PIN set",
                Status::PinRequired => "the security key requires a PIN",
                Status::PinPolicy => "the PIN does not meet the security key's policy",
                Status::NoCredential => "this security key is not enrolled in the vault",
                Status::CredentialExcluded => "this security key is already enrolled",
                Status::TouchTimeout | Status::ActionTimeout => {
                    "the security key was not touched in time"
                }
                Status::Cancelled | Status::Denied | Status::Other(_) => {
                    "the security key refused the operation"
                }
            },
        },
    }
}

/// An entry as the notebook list shows it.
pub struct Summary<'a> {
    pub id: EntryId,
    pub revision: u64,
    pub title: &'a str,
}

/// An entry's whole stored text.
pub struct Entry<'a> {
    pub id: EntryId,
    pub revision: u64,
    pub title: &'a str,
    pub body: &'a [u8],
}

/// A change against the revision the caller last read. Its text is
/// cleared when it is dropped: unapplied, refused or saved.
pub enum Change {
    Create {
        title: String,
        body: String,
    },
    Edit {
        id: EntryId,
        base: u64,
        title: String,
        body: String,
    },
    Rename {
        id: EntryId,
        base: u64,
        title: String,
    },
    Delete {
        id: EntryId,
        base: u64,
    },
}

impl Drop for Change {
    fn drop(&mut self) {
        match self {
            Self::Create { title, body } | Self::Edit { title, body, .. } => {
                wipe(title);
                wipe(body);
            }
            Self::Rename { title, .. } => wipe(title),
            Self::Delete { .. } => {}
        }
    }
}

fn wipe(text: &mut String) {
    let mut bytes = std::mem::take(text).into_bytes();
    bytes.fill(0);
    // Keeps the clearing from being removed as a dead store.
    std::hint::black_box(&mut bytes);
}

impl From<Change> for notebook::Change {
    fn from(mut change: Change) -> Self {
        // The text moves into clearing owners; what drops here is empty.
        let take = std::mem::take;
        match &mut change {
            Change::Create { title, body } => Self::Create {
                title: Text::new(take(title)),
                body: Text::new(take(body)),
            },
            Change::Edit {
                id,
                base,
                title,
                body,
            } => Self::Edit {
                id: *id,
                base: *base,
                title: Text::new(take(title)),
                body: Text::new(take(body)),
            },
            Change::Rename { id, base, title } => Self::Rename {
                id: *id,
                base: *base,
                title: Text::new(take(title)),
            },
            Change::Delete { id, base } => Self::Delete {
                id: *id,
                base: *base,
            },
        }
    }
}

/// The identity and revision a change committed; a delete has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    pub id: EntryId,
    pub revision: Option<u64>,
}

/// An unlocked notebook. Dropping it is the lock: the session's key and
/// plaintext go with it.
pub struct Vault(Session);

impl Vault {
    /// Entries in stored order.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = Summary<'_>> {
        self.0.entries().map(|entry| Summary {
            id: entry.id,
            revision: entry.revision,
            title: entry.title,
        })
    }

    pub fn entry(&self, id: &EntryId) -> Option<Entry<'_>> {
        self.0.entry(id).map(|entry| Entry {
            id: entry.id,
            revision: entry.revision,
            title: entry.title(),
            body: entry.body(),
        })
    }

    /// The vault revision this session holds.
    pub fn revision(&self) -> u64 {
        self.0.revision()
    }

    /// The keys enrolled in the revision this session holds.
    pub fn keys(&self) -> Vec<Key> {
        self.0.keys().into_iter().map(Key::from).collect()
    }

    /// The key that authorizes this session's saves.
    pub fn key(&self) -> Option<Key> {
        self.keys()
            .into_iter()
            .find(|key| key.credential == self.0.credential())
    }

    /// Authorizes later saves with another enrolled key, one of `keys`.
    pub fn use_key(&mut self, key: &Key) -> Result<(), Failure> {
        self.0.use_key(&key.credential).map_err(Failure)
    }

    /// The authenticated ciphertext of the revision this session holds,
    /// for an encrypted copy; refused after an uncertain save.
    pub fn export(&self) -> Result<Vec<u8>, Failure> {
        self.0.ciphertext().map(<[u8]>::to_vec).map_err(Failure)
    }
}

/// The largest exported copy; a caller reading one for `keys_of` or
/// `Host::import` reads at most this many bytes plus one, and refuses a
/// copy that reaches the extra byte without reading further.
pub const MAX_COPY: usize = super::MAX_ENVELOPE;

/// The keys enrolled in an exported copy, read without authenticating it,
/// so a person can choose which one to import it with.
pub fn keys_of(copy: &[u8]) -> Result<Vec<Key>, Failure> {
    let vault = LockedVault::decode(copy).map_err(|reason| Failure(Error::Refused(reason)))?;
    Ok(vault
        .unlock_hints()
        .map(|hint| Key {
            role: hint.role.into(),
            credential: hint.credential.to_vec(),
        })
        .collect())
}

/// This desktop account's vault in standalone mode: the protected
/// process, the vault directory and the kernel's entropy. Choosing
/// standalone mode is the caller's: on td the notebook uses the admitted
/// service instead, and no failed service request may lead here.
pub struct Host {
    protected: Protected,
    directory: Directory,
    random: Entropy,
    tokens: Entropy,
}

impl Host {
    /// Protects the process and admits the account's vault directory; a
    /// host that cannot keep PINs and keys out of dumps and swap is
    /// refused here.
    pub fn open() -> Result<Self, Failure> {
        let protected = host::protect().map_err(|reason| Failure(Error::Refused(reason)))?;
        let directory = host::account_directory().map_err(Failure)?;
        let entropy = || Entropy::open().map_err(|reason| Failure(Error::Refused(reason)));
        Ok(Self {
            protected,
            directory,
            random: entropy()?,
            tokens: entropy()?,
        })
    }

    /// The enrolled keys, or `None` when no vault exists yet.
    pub fn keys(&self) -> Result<Option<Vec<Key>>, Failure> {
        if !lifecycle::exists(&self.directory).map_err(Failure)? {
            return Ok(None);
        }
        let keys = lifecycle::keys(&self.directory).map_err(Failure)?;
        Ok(Some(keys.into_iter().map(Key::from).collect()))
    }

    /// Creates the vault, enrolling the primary and then the backup key. A
    /// cancel after publication began cannot stop it: the vault exists, and
    /// the `Vault` returned is the caller's to drop when it has locked.
    pub fn create(&mut self, prompt: &mut dyn Prompt, cancel: &Cancel) -> Result<Vault, Failure> {
        let Self {
            protected,
            directory,
            random,
            tokens,
        } = self;
        with_tokens(protected, tokens, prompt, cancel, |hardware| {
            lifecycle::create(directory, hardware, &mut Random(random))
        })
        .map(Vault)
    }

    /// Unlocks the vault for browsing with `key`, one of `keys`. A cancel
    /// that arrives before the unlocked session is returned drops it, and
    /// the unlock ends as cancelled.
    pub fn unlock(
        &mut self,
        key: &Key,
        prompt: &mut dyn Prompt,
        cancel: &Cancel,
    ) -> Result<Vault, Failure> {
        let Self {
            protected,
            directory,
            random,
            tokens,
        } = self;
        let vault = with_tokens(protected, tokens, prompt, cancel, |hardware| {
            lifecycle::unlock(directory, &key.credential, hardware, &mut Random(random))
        })
        .map(Vault)?;
        if cancel.0.cancelled() {
            drop(vault);
            return Err(Failure(cancelled()));
        }
        Ok(vault)
    }

    /// Places an exported `copy`, authenticated with `key`, one of
    /// `keys_of(copy)`, into this account when it holds no vault. As with
    /// `create`, a `Vault` returned after a late cancel is the caller's to
    /// drop.
    pub fn import(
        &mut self,
        copy: &[u8],
        key: &Key,
        prompt: &mut dyn Prompt,
        cancel: &Cancel,
    ) -> Result<Vault, Failure> {
        let Self {
            protected,
            directory,
            random,
            tokens,
        } = self;
        with_tokens(protected, tokens, prompt, cancel, |hardware| {
            lifecycle::import(
                directory,
                copy,
                &key.credential,
                hardware,
                &mut Random(random),
            )
        })
        .map(Vault)
    }

    /// Enrolls another backup key after authorization by the key `vault`
    /// was unlocked with.
    pub fn add_key(
        &mut self,
        vault: &mut Vault,
        prompt: &mut dyn Prompt,
        cancel: &Cancel,
    ) -> Result<(), Failure> {
        let Self {
            protected,
            directory,
            random,
            tokens,
        } = self;
        with_tokens(protected, tokens, prompt, cancel, |hardware| {
            vault.0.add_key(directory, hardware, &mut Random(random))
        })
    }

    /// Revokes `revoked`, keys of `vault.keys()`, and enrolls one
    /// replacement, with every retained key taking part; the vault key
    /// rotates and no revoked key opens the result.
    pub fn replace_keys(
        &mut self,
        vault: &mut Vault,
        revoked: &[Key],
        prompt: &mut dyn Prompt,
        cancel: &Cancel,
    ) -> Result<(), Failure> {
        let revoked: Vec<&[u8]> = revoked.iter().map(|key| &key.credential[..]).collect();
        let Self {
            protected,
            directory,
            random,
            tokens,
        } = self;
        with_tokens(protected, tokens, prompt, cancel, |hardware| {
            vault
                .0
                .replace_keys(directory, &revoked, hardware, &mut Random(random))
        })
    }

    /// Saves one change under fresh authorization by the key `vault` was
    /// unlocked with. Refusals of the change come before any token.
    pub fn apply(
        &mut self,
        vault: &mut Vault,
        change: Change,
        prompt: &mut dyn Prompt,
        cancel: &Cancel,
    ) -> Result<Committed, Failure> {
        let Self {
            protected,
            directory,
            random,
            tokens,
        } = self;
        with_tokens(protected, tokens, prompt, cancel, |hardware| {
            vault
                .0
                .apply(directory, change.into(), hardware, &mut Random(random))
        })
        .map(|committed| Committed {
            id: committed.id,
            revision: committed.revision,
        })
    }
}

/// Runs `operation` over the production token adapter: each presentation
/// asks `prompt` which key to connect, opens the one connected token
/// under `cancel`, and takes its PIN from `prompt`; `cancel` is checked
/// again before any publication.
fn with_tokens<T>(
    protected: &Protected,
    entropy: &mut Entropy,
    prompt: &mut dyn Prompt,
    cancel: &Cancel,
    operation: impl FnOnce(&mut Erased<'_>) -> Result<T, Error>,
) -> Result<T, Failure> {
    // Opening and the PIN prompt both reach the caller's prompt, one after
    // the other and never nested.
    let prompt = RefCell::new(prompt);
    // The transaction reports any PIN prompt error as PIN input; one the
    // prompt itself returned is the person declining.
    let declined = Cell::new(false);
    let decline = || TokenError::Failed(Transaction::Interrupted(Interruption::Cancelled));
    let mut hardware = Hardware {
        open: |presented: Presented<'_>| {
            if cancel.0.cancelled() {
                return Err(decline());
            }
            prompt
                .try_borrow_mut()
                .map_err(|_| TokenError::Host("the key prompt is already in use"))?
                .present(presented.into())
                .map_err(|_| decline())?;
            // A cancel during the presentation or the token's startup is
            // the cancellation, not whatever startup then failed with.
            if cancel.0.cancelled() {
                return Err(decline());
            }
            host::open(protected, presented, &cancel.0).map_err(|error| {
                if cancel.0.cancelled() {
                    decline()
                } else {
                    error
                }
            })
        },
        prompt: |presented: Presented<'_>, purpose: PinPurpose| {
            let purpose = match purpose {
                PinPurpose::Assertion => PinUse::Authorize,
                PinPurpose::Creation => PinUse::Enroll,
                PinPurpose::EnrollmentProof => PinUse::Proof,
            };
            let bytes = prompt
                .try_borrow_mut()
                .map_err(|_| String::from("the PIN prompt is already in use"))?
                .pin(presented.into(), purpose)
                .inspect_err(|_| declined.set(true))?;
            Pin::new(bytes)
        },
        entropy: |bytes: &mut [u8]| entropy.fill(bytes),
    };
    let result = operation(&mut Erased {
        tokens: &mut hardware,
        cancel,
    });
    result.map_err(|error| match error {
        Error::Token(TokenError::Failed(Transaction::PinInput)) if declined.get() => {
            Failure(cancelled())
        }
        error => Failure(error),
    })
}

/// The adapter with its closure types erased, as the lifecycle's generic
/// operations take it, answering revocation from the caller's `Cancel`.
struct Erased<'a> {
    tokens: &'a mut dyn Tokens,
    cancel: &'a Cancel,
}

impl Tokens for Erased<'_> {
    fn enroll(
        &mut self,
        presented: Presented<'_>,
        request: EnrollRequest<'_>,
    ) -> Result<Enrolled, TokenError> {
        self.tokens.enroll(presented, request)
    }

    fn assert(
        &mut self,
        presented: Presented<'_>,
        request: AssertRequest<'_>,
    ) -> Result<Asserted, TokenError> {
        self.tokens.assert(presented, request)
    }

    fn revoked(&self) -> bool {
        self.cancel.0.cancelled()
    }
}

/// The kernel's entropy as the lifecycle reads it.
struct Random<'a>(&'a mut Entropy);

impl Read for Random<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.fill(buffer).map_err(io::Error::other)?;
        Ok(buffer.len())
    }
}

/// The token worker a presentation re-executes this binary as. `args` are
/// the arguments after the program name, as `run` takes them; a program
/// linking this API calls it first and exits with its result when it
/// answers.
pub fn worker(args: &[String]) -> Option<Result<(), String>> {
    match args {
        [role, index, inode, rdev, runtime] if role == "hid-worker-desktop" => {
            Some(fido_device::desktop_worker(index, inode, rdev, runtime))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::lifecycle::Purpose;
    use super::*;

    fn every_failure() -> Vec<Error> {
        let mut errors = vec![
            Error::Store("busy".into()),
            Error::State("no portable vault exists"),
            Error::Refused("bad".into()),
            Error::Uncertain,
            Error::Token(TokenError::Unavailable),
            Error::Token(TokenError::Denied),
            Error::Token(TokenError::Several),
            Error::Token(TokenError::Host("swap or core dumps became enabled")),
        ];
        for entry in [
            EntryError::Missing,
            EntryError::Stale,
            EntryError::Title,
            EntryError::Duplicate,
            EntryError::Body,
            EntryError::Full,
        ] {
            errors.push(Error::Entry(entry));
        }
        for failed in [
            Transaction::Interrupted(Interruption::Cancelled),
            Transaction::Interrupted(Interruption::Expired),
            Transaction::Interrupted(Interruption::Closed),
            Transaction::Transport,
            Transaction::Protocol("pin token 1234 rejected".into()),
            Transaction::PinInput,
            Transaction::Entropy,
        ] {
            errors.push(Error::Token(TokenError::Failed(failed)));
        }
        for status in [
            Status::CredentialExcluded,
            Status::Denied,
            Status::Cancelled,
            Status::NoCredential,
            Status::TouchTimeout,
            Status::PinInvalid,
            Status::PinBlocked,
            Status::PinAuthInvalid,
            Status::PinAuthBlocked,
            Status::PinNotSet,
            Status::PinRequired,
            Status::PinPolicy,
            Status::ActionTimeout,
            Status::Other(0x7f),
        ] {
            errors.push(Error::Token(TokenError::Failed(Transaction::Status(
                status,
            ))));
        }
        errors
    }

    #[test]
    fn failure_text_names_a_reason_and_never_token_output() {
        for error in every_failure() {
            let failure = Failure(error);
            let text = failure.to_string();
            assert!(!text.is_empty());
            // A protocol detail is the token's answer: it is not shown,
            // and Debug shows only the same text.
            assert!(!text.contains("1234"), "{text}");
            assert_eq!(format!("{failure:?}"), format!("Failure({text:?})"));
        }
        assert_eq!(
            Failure(Error::Token(TokenError::Failed(Transaction::PinInput))).to_string(),
            "a PIN is 4 to 63 printable ASCII characters"
        );
        let text = |error| Failure(error).to_string();
        assert_eq!(
            text(Error::Token(TokenError::Failed(Transaction::Status(
                Status::PinInvalid
            )))),
            "wrong PIN"
        );
        assert_eq!(
            text(Error::Token(TokenError::Several)),
            "more than one security key is connected; connect only one"
        );
        assert_eq!(
            text(Error::State("no portable vault exists")),
            "no portable vault exists"
        );
    }

    #[test]
    fn failure_predicates_name_exactly_their_case() {
        let cancelled = Error::Token(TokenError::Failed(Transaction::Interrupted(
            Interruption::Cancelled,
        )));
        for error in every_failure() {
            let failure = Failure(error);
            assert_eq!(
                failure.stale(),
                failure.0 == Error::Entry(EntryError::Stale)
            );
            assert_eq!(failure.uncertain(), failure.0 == Error::Uncertain);
            assert_eq!(failure.cancelled(), failure.0 == cancelled);
        }
    }

    #[test]
    fn a_fingerprint_is_the_credential_digest_prefix() {
        let credential = b"credential one";
        let digest = crate::crypto::digest(credential);
        let expected: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(Fingerprint::of(credential).to_string(), expected);
        assert_eq!(expected.len(), 8);
        assert_ne!(Fingerprint::of(b"one"), Fingerprint::of(b"two"));
        let key = Key {
            role: KeyRole::Backup,
            credential: credential.to_vec(),
        };
        assert_eq!(key.fingerprint(), Fingerprint::of(credential));
        assert_eq!(key.role(), KeyRole::Backup);
    }

    #[test]
    fn a_request_names_the_operation_role_and_enrolled_key() {
        let request = Request::from(Presented {
            purpose: Purpose::Unlock,
            role: Role::Primary,
            credential: Some(b"enrolled"),
        });
        assert_eq!(request.operation, Purpose::Unlock.label());
        assert_eq!(request.role, KeyRole::Primary);
        assert_eq!(request.key, Some(Fingerprint::of(b"enrolled")));
        let enrolling = Request::from(Presented {
            purpose: Purpose::CreateBackup,
            role: Role::Backup,
            credential: None,
        });
        assert_eq!((enrolling.role, enrolling.key), (KeyRole::Backup, None));
    }

    #[test]
    fn an_undecodable_copy_has_no_keys() {
        for copy in [&b""[..], b"TDVAULT2", &[0; 64]] {
            assert!(keys_of(copy).is_err());
        }
    }

    #[test]
    fn only_the_desktop_worker_argv_is_answered() {
        let args = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        for other in [
            args(&[]),
            args(&["hid-worker", "1", "2", "3"]),
            args(&["hid-worker-desktop", "1", "2", "3"]),
            args(&["hid-worker-desktop", "1", "2", "3", "/run", "extra"]),
            args(&["--help"]),
        ] {
            assert!(worker(&other).is_none(), "{other:?}");
        }
    }

    #[test]
    fn a_change_keeps_its_identity_base_and_text() {
        let id = [7; 16];
        match notebook::Change::from(Change::Edit {
            id,
            base: 3,
            title: "t".into(),
            body: "b".into(),
        }) {
            notebook::Change::Edit {
                id: changed, base, ..
            } => assert_eq!((changed, base), (id, 3)),
            _ => panic!("an edit stays an edit"),
        }
        assert!(matches!(
            notebook::Change::from(Change::Delete { id, base: 9 }),
            notebook::Change::Delete { base: 9, .. }
        ));
        assert!(matches!(
            notebook::Change::from(Change::Rename {
                id,
                base: 1,
                title: "r".into()
            }),
            notebook::Change::Rename { base: 1, .. }
        ));
        assert!(matches!(
            notebook::Change::from(Change::Create {
                title: "c".into(),
                body: String::new()
            }),
            notebook::Change::Create { .. }
        ));
    }
}
