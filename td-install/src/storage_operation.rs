//! `td-install storage-operation --uid 1000`: td-authd's root storage
//! worker (DESIGN.md "Storage operation worker", td-authd/DESIGN.md
//! amendment 10). In increment 8e it carries the selector update in both
//! forms and the keyslot test of a login-key operation's disk proof, and in
//! 8f the re-encrypting upgrade's phase 1 and its cancellation; td-authd
//! starts it only in test builds until ENCRYPTION.md's item 9, so nothing
//! in production runs it.
//!
//! The worker reads everything an update needs before it presents the
//! description, so the approval key's time holds no I/O, and after root's
//! commit acknowledgement it re-reads the header, the deployment's
//! standing and the ESP's `EFI/BOOT` sector before its first write: a
//! change there is `28`, definite because nothing was written, and the
//! worker exits successfully after sending it. A disk proof writes
//! nothing: it presents the proof's steps, takes the recovery key or a
//! disk security key's assertion, and tests the keyslot that secret opens
//! against an unchanged header. Phase 1 presents its review and each new
//! key's ceremony, then writes key-less header commits and the selector;
//! the cancellation removes what phase 1 wrote.

use crate::consent::{
    upgrade_ceiling, DiskProofs, LoginStep, Operation, Proof, Request, SelectorForm, UpgradeShape,
    UpgradeStep, LOGIN_LONGEST, SELECTOR_UPDATE_CEILING, UPGRADE_CANCEL_CEILING,
};
use crate::frames::{self, remaining, Wire};
use crate::{
    esp_region, inventory, logical_sector_size, paths, protocol, read_trusted_key, sha256, td_fs,
    MAX_BOOT_FILE,
};
use std::fs::File;
use std::io::{self, Read as _, Seek, SeekFrom};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use td_fido::fido_ctap::{fingerprint, Es256PublicKey};
use td_fido::fido_device::{self, Interruption, Program, MAX_LIFETIME};
use td_fido::fido_disk::{self, CreateStep, CreateSteps, Created, DiskError, Released, Steps};
use td_fido::fido_p256::PublicKey;
use td_fido::fido_pin::{LoginRefusal, Pin};
use td_fido::fido_transaction::{Devices, Error as FidoError, Status};
use td_protector::cryptsetup::{self, Bounded, Cryptsetup};
use td_protector::lockout::volume_uuid;
use td_protector::luks2::{self, Header, Kind, UpgradeView};
use td_protector::protected::{Fido2, FidoRole, Shape, Token};
use td_protector::protected_plan;
use td_protector::recovery::RecoveryKey;
use td_protector::upgrade::{self, Step};

const PRESENT: u8 = 0x10;
const PRESENTED: u8 = 0x11;
const COMMIT: u8 = 0x12;
const COMMITTED: u8 = 0x13;
const SUCCESS: u8 = 0x14;
const FAILURE: u8 = 0x15;
const BASELINE: u8 = 0x18;
/// What the commit invitation must leave of the deadline: the writes'.
pub(crate) const COMMIT_MARGIN: Duration = Duration::from_secs(120);

/// Why an operation failed, as its `15` frame carries it. Root ends a
/// selector update with `20` to `22` from the baseline itself; the worker
/// sends `20` only from a disk proof's re-read of the header. A disk key's
/// kinds are td-secret's login worker's (td-secret/DESIGN.md, "Login-key
/// worker").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The key's retry count as it reported it before this attempt.
    WrongPin(u8),
    PinAuthBlocked,
    PinBlocked,
    /// The connected key holds none of the header's fido2 credentials.
    NotEnrolled,
    /// None or several FIDO devices, or another than identify's.
    OneKey,
    Refused(LoginRefusal),
    /// The key denied presence.
    Denied,
    Timeout,
    Failed,
    Internal,
    /// The held header is gone, or td's reader now refuses it.
    Volume,
    Deployment,
    Esp,
    /// The recovery-key codec refused the entry.
    Mistyped,
    /// The keyslot refused the recovery key or the derived passphrase.
    WrongKey,
    /// After root's `13` and before any write, or before a disk proof's
    /// keyslot test.
    Changed,
    /// A new key already holds a credential the upgrade excludes.
    Excluded,
    /// The new credential's identify probe did not select it.
    Unprobed,
    /// A volume state the operation does not take: a header not converged
    /// for an upgrade, one past U0 for a cancellation.
    State,
    /// The TPM shape's TPM, as its detail says.
    Tpm(TpmRefusal),
}

/// Why the TPM shape's one TPM read refused it, `27`'s detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TpmRefusal {
    /// No TPM, or no answer.
    Absent,
    NoSha256Bank,
    /// `lockoutAuthSet`, which a device-bound volume holds no token to
    /// prove.
    LockoutSet,
}

impl Failure {
    pub(crate) fn kind(self) -> u8 {
        match self {
            Self::WrongPin(_) => 0x01,
            Self::PinAuthBlocked => 0x02,
            Self::PinBlocked => 0x03,
            Self::NotEnrolled => 0x04,
            Self::OneKey => 0x05,
            Self::Refused(_) => 0x06,
            Self::Denied => 0x07,
            Self::Timeout => 0x08,
            Self::Failed => 0x0f,
            Self::Internal => 0x10,
            Self::Volume => 0x20,
            Self::Deployment => 0x23,
            Self::Esp => 0x24,
            Self::Mistyped => 0x25,
            Self::WrongKey => 0x26,
            Self::Changed => 0x28,
            Self::Excluded => 0x11,
            Self::Unprobed => 0x06,
            Self::State => 0x21,
            Self::Tpm(_) => 0x27,
        }
    }

    /// `15`, the kind, and the detail byte a wrong PIN and a refusal carry,
    /// as the login worker frames them.
    pub(crate) fn frame(self) -> Vec<u8> {
        let detail = match self {
            Self::WrongPin(retries) => Some(retries),
            Self::Refused(refusal) => Some(match refusal {
                LoginRefusal::NoHmacSecret => 1,
                LoginRefusal::AlwaysUv => 2,
                LoginRefusal::PinUnsupported => 3,
                LoginRefusal::PinNotSet => 4,
                LoginRefusal::ListTooSmall => 5,
                // A key that cannot require its PIN has no typed text.
                LoginRefusal::NoFido21 | LoginRefusal::PinOptional => return Self::Failed.frame(),
            }),
            // The login worker's reserved detail for a probe that fails.
            Self::Unprobed => Some(6),
            Self::Tpm(refusal) => Some(match refusal {
                TpmRefusal::Absent => 1,
                TpmRefusal::NoSha256Bank => 2,
                TpmRefusal::LockoutSet => 3,
            }),
            _ => None,
        };
        let mut frame = vec![FAILURE, self.kind()];
        frame.extend(detail);
        frame
    }
}

/// The baseline's volume byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Volume {
    None,
    DeviceBound,
    Upgrading,
    TpmPrimary,
    Fido2Primary,
    Refused,
}

impl Volume {
    fn byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::DeviceBound => 1,
            Self::Upgrading => 2,
            Self::TpmPrimary => 3,
            Self::Fido2Primary => 4,
            Self::Refused => 5,
        }
    }

    /// The selector update's form on this volume; none where it takes none.
    fn form(self) -> Option<SelectorForm> {
        match self {
            Self::DeviceBound => Some(SelectorForm::DeviceBound),
            Self::TpmPrimary => Some(SelectorForm::Staged),
            Self::Fido2Primary => Some(SelectorForm::Fido2),
            Self::None | Self::Upgrading | Self::Refused => None,
        }
    }

    /// A protected volume's selector carries the deployment measurement
    /// policy (DESIGN.md "Selector deployment measurement prerequisite").
    fn measured(self) -> bool {
        matches!(self, Self::TpmPrimary | Self::Fido2Primary)
    }
}

/// What discovery's held partition gave: no encrypted td volume, or its
/// LUKS2 header as td-protector's reader read it.
pub(crate) type Read = Option<Result<Header, String>>;

/// The baseline from a header read and the running deployment's standing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Baseline {
    pub(crate) volume: Volume,
    recovery: bool,
    tokens: Vec<(FidoRole, [u8; 4])>,
    pub(crate) current: Option<[u8; 32]>,
}

impl Baseline {
    pub(crate) fn new(read: &Read, current: Option<[u8; 32]>) -> Self {
        let mut baseline = Self {
            volume: Volume::None,
            recovery: false,
            tokens: Vec::new(),
            current,
        };
        let header = match read {
            None => return baseline,
            Some(Err(_)) => {
                baseline.volume = Volume::Refused;
                return baseline;
            }
            Some(Ok(header)) => header,
        };
        baseline.volume = match header.kind() {
            Kind::DeviceBound => Volume::DeviceBound,
            Kind::Upgrading => Volume::Upgrading,
            Kind::Protected => match protected_plan::check(header) {
                Ok(Shape::Tpm) => Volume::TpmPrimary,
                Ok(Shape::Fido2) => Volume::Fido2Primary,
                Err(_) => Volume::Refused,
            },
        };
        baseline.recovery = protected_plan::marker(header).is_some();
        // The same tokens, in the same order, a disk proof identifies among.
        baseline.tokens = fido_tokens(header)
            .into_iter()
            .map(|fido| (fido.role, fingerprint(&fido.credential)))
            .collect();
        baseline
    }

    /// `18`, the volume byte, the recovery-key byte, the tokens with their
    /// roles and fingerprints, then `00` and the manifest ID, or `01`.
    pub(crate) fn frame(&self) -> Vec<u8> {
        let mut frame = vec![
            BASELINE,
            self.volume.byte(),
            u8::from(self.recovery),
            // At most four.
            self.tokens.len() as u8,
        ];
        for (role, fingerprint) in &self.tokens {
            frame.push(match role {
                FidoRole::Primary => 1,
                FidoRole::Recovery => 2,
            });
            frame.extend_from_slice(fingerprint);
        }
        match self.current {
            Some(id) => {
                frame.push(0);
                frame.extend_from_slice(&id);
            }
            None => frame.push(1),
        }
        frame
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            // Writing to a String cannot fail.
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// What the worker reads and writes, the machine's in production and a
/// test's built header, deployment and ESP image in host tests.
pub(crate) trait Host {
    type Disk: td_fat::Device;
    /// The held partition's header, read afresh each call.
    fn header(&mut self) -> Read;
    /// The volume UUID `td.volume=` names.
    fn uuid(&self) -> &str;
    /// The running deployment's manifest ID when it is the volume's
    /// `current`, acknowledged, with no pending attempt.
    fn standing(&mut self) -> Option<[u8; 32]>;
    /// `current`'s `bzImage`, after its manifest is verified under the
    /// trusted key and the kernel hashed against it.
    fn kernel(&mut self, id: &[u8; 32]) -> Result<Vec<u8>, String>;
    /// The running root's selector template.
    fn template(&mut self) -> Result<Vec<u8>, String>;
    /// The trusted key as the volume holds it.
    fn trusted_key(&mut self) -> Result<Vec<u8>, String>;
    /// The held disk's ESP, admitted by td-fat.
    fn esp(&mut self) -> Result<td_fat::Esp<Self::Disk>, String>;
    /// `cryptsetup open --test-passphrase` of keyslot `slot`, `key` on
    /// standard input, which maps nothing, against a private detached
    /// copy of the held partition's header area that must read as
    /// `expected`: a repair cryptsetup makes lands in the copy, never on
    /// the disk. The child lives until `deadline` at most. Any failure but
    /// these outcomes is an error naming no key.
    fn test_keyslot(
        &mut self,
        slot: u8,
        key: &[u8],
        expected: &Header,
        deadline: Instant,
    ) -> Result<Tested, String>;
    /// The held partition's header and its upgrade view, read afresh.
    fn upgrade_view(&mut self) -> Option<Result<UpgradeView, String>>;
    /// The TPM shape's one TPM read: a SHA-256 bank, and `lockoutAuthSet`
    /// clear in `TPM_PT_PERMANENT`. It writes nothing.
    fn tpm(&mut self) -> Result<(), TpmRefusal>;
    /// `luksDump --dump-json-metadata` of the held partition, after root's
    /// `13`: cryptsetup may repair the header it reads.
    fn metadata(&mut self) -> Result<Vec<u8>, String>;
    /// One key-less header commit on the held partition, a token's removal
    /// or import, whose child lives until `deadline` at most.
    fn commit(&mut self, step: &Step, deadline: Instant) -> Result<(), String>;
}

/// What a keyslot test found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Tested {
    Opened,
    /// cryptsetup's wrong-passphrase exit.
    Refused,
    /// The header area no longer reads as the one presented, before or
    /// while it was copied; nothing ran.
    Changed,
    /// The deadline passed first; the child was killed and reaped.
    Expired,
}

/// The disk security key a proof asks: td-fido's two-step authorization
/// over root hidraw in production, a script in host tests.
pub(crate) trait Keys {
    fn authorize(
        &mut self,
        deadline: Instant,
        tokens: &[fido_disk::Token<'_>],
        volume: &[u8; 16],
        identify: &[u8],
        steps: &mut Presenter<'_>,
    ) -> Result<Released, DiskError>;
    /// After a connect step: waits until `deadline` for exactly one key,
    /// not the previous ceremony's. Several refuse.
    fn connect(&mut self, deadline: Instant) -> Result<(), Failure>;
    /// A new key's ceremony through td-fido's stepped creation.
    fn create(
        &mut self,
        deadline: Instant,
        excluded: &[&[u8]],
        volume: &[u8; 16],
        steps: &mut Ceremony<'_>,
    ) -> Result<Created, DiskError>;
}

/// The selector `prepare` builds from the template, key and identity.
pub(crate) type Prepare<'a> = &'a dyn Fn(&[u8], &[u8], &str, bool) -> Result<Vec<u8>, String>;

/// The operation, then its failure frame, best effort. A definite `28`
/// is the one failure the worker exits successfully after.
#[allow(clippy::too_many_arguments)]
pub(crate) fn operate<H: Host, K: Keys>(
    wire: &mut Wire,
    host: &mut H,
    keys: &mut K,
    uid: u32,
    started: Instant,
    margin: Duration,
    prepare: Prepare<'_>,
    entropy: &mut dyn FnMut() -> Result<[u8; 32], String>,
) -> Result<(), Failure> {
    let result = perform(wire, host, keys, uid, started, margin, prepare, entropy);
    wire.session(None);
    match result {
        Ok(()) => Ok(()),
        Err(failure) => {
            let _ = wire.send(&failure.frame());
            if failure == Failure::Changed {
                Ok(())
            } else {
                Err(failure)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn perform<H: Host, K: Keys>(
    wire: &mut Wire,
    host: &mut H,
    keys: &mut K,
    uid: u32,
    started: Instant,
    margin: Duration,
    prepare: Prepare<'_>,
    entropy: &mut dyn FnMut() -> Result<[u8; 32], String>,
) -> Result<(), Failure> {
    let read = host.header();
    let baseline = Baseline::new(&read, host.standing());
    wire.send(&baseline.frame()).map_err(|_| framing(wire))?;
    let frame = wire.receive_cleared().map_err(|_| framing(wire))?;
    let request = Request::decode(frame.bytes()).map_err(|_| Failure::Internal)?;
    drop(frame);
    if request.owner() != uid {
        return Err(Failure::Internal);
    }
    if request.login_proof().is_some() {
        let proof = Proved {
            request: &request,
            baseline: &baseline,
            read: &read,
        };
        return proof.run(wire, host, keys, started, entropy);
    }
    let operation = Operated {
        request: &request,
        baseline: &baseline,
        read: &read,
    };
    match request.operation() {
        Operation::Upgrade { .. } => {
            return operation.upgrade(wire, host, keys, started, margin, prepare, entropy)
        }
        Operation::UpgradeCancel { .. } => {
            return operation.cancel(wire, host, started, margin, entropy)
        }
        _ => {}
    }
    // Root derives the description from the baseline, as this does.
    let (form, id) = match (
        request.operation(),
        baseline.volume.form(),
        baseline.current,
    ) {
        (Operation::SelectorUpdate { form, current, .. }, Some(expected), Some(id))
            if *form == expected && *current == hex(&id) =>
        {
            (*form, id)
        }
        _ => return Err(Failure::Internal),
    };
    wire.limit(
        started
            .checked_add(SELECTOR_UPDATE_CEILING)
            .ok_or(Failure::Internal)?,
    );
    // Every read before the presentation.
    let staged = form == SelectorForm::Staged;
    let pair = Pair::read(host, &id, baseline.volume.measured(), staged, prepare)?;
    // The description, presented once; its typed key is its receipt.
    let description = request.encode();
    let round = entropy().map_err(|_| Failure::Internal)?;
    let bound = wire.bound();
    wire.exchange(PRESENT, PRESENTED, round, &description, Some(bound))
        .map_err(|_| framing(wire))?;
    if remaining(wire.deadline()).map_err(|_| Failure::Timeout)? < margin {
        return Err(Failure::Timeout);
    }
    let round = entropy().map_err(|_| Failure::Internal)?;
    wire.exchange(COMMIT, COMMITTED, round, &description, None)
        .map_err(|_| framing(wire))?;
    // Root may now count a write as begun; nothing is written until every
    // read below agrees with what was presented.
    if host.header() != read || host.standing() != baseline.current {
        return Err(Failure::Changed);
    }
    let mut esp = pair.reopen(host)?;
    pair.write(&mut esp)?;
    wire.send(&[SUCCESS]).map_err(|_| Failure::Internal)
}

/// A selector pair's reads before the presentation: `current`'s
/// authenticated kernel, the selector prepared from the running root's
/// template, and the ESP's `EFI/BOOT` sector as td-fat admitted it, which
/// fits the pair in its form.
struct Pair {
    kernel: Vec<u8>,
    selector: Vec<u8>,
    staged: bool,
    admitted: td_fat::BootDir,
}

impl Pair {
    fn read<H: Host>(
        host: &mut H,
        id: &[u8; 32],
        measured: bool,
        staged: bool,
        prepare: Prepare<'_>,
    ) -> Result<Self, Failure> {
        let kernel = host.kernel(id).map_err(|_| Failure::Deployment)?;
        let template = host.template().map_err(|_| Failure::Deployment)?;
        let key = host.trusted_key().map_err(|_| Failure::Internal)?;
        let selector =
            prepare(&template, &key, host.uuid(), measured).map_err(|_| Failure::Deployment)?;
        drop(template);
        let esp = host.esp().map_err(|_| Failure::Esp)?;
        esp.fits(kernel.len() as u64, selector.len() as u64, staged)
            .map_err(|_| Failure::Esp)?;
        let admitted = esp.boot().clone();
        Ok(Self {
            kernel,
            selector,
            staged,
            admitted,
        })
    }

    /// The ESP again after root's `13`, its sector as admitted, or `28`.
    fn reopen<H: Host>(&self, host: &mut H) -> Result<td_fat::Esp<H::Disk>, Failure> {
        let esp = host.esp().map_err(|_| Failure::Changed)?;
        if *esp.boot() != self.admitted {
            return Err(Failure::Changed);
        }
        // `fits` writes nothing, so a refusal now is as definite as the
        // reads': the ESP's free space or slots changed since the
        // presentation.
        esp.fits(
            self.kernel.len() as u64,
            self.selector.len() as u64,
            self.staged,
        )
        .map_err(|_| Failure::Changed)?;
        Ok(esp)
    }

    fn write<D: td_fat::Device>(&self, esp: &mut td_fat::Esp<D>) -> Result<(), Failure> {
        let input = |bytes: &[u8]| bytes.len() as u64;
        let (mut k, mut i) = (self.kernel.as_slice(), self.selector.as_slice());
        let kernel = td_fat::Input {
            len: input(k),
            reader: &mut k,
        };
        let initrd = td_fat::Input {
            len: input(i),
            reader: &mut i,
        };
        if self.staged {
            esp.stage(kernel, initrd)
        } else {
            esp.write_direct(kernel, initrd)
        }
        .map_err(|_| Failure::Failed)
    }
}

/// An upgrade or its cancellation: root's first description, the baseline
/// it was built from, and the header read that baseline holds.
struct Operated<'a> {
    request: &'a Request,
    baseline: &'a Baseline,
    read: &'a Read,
}

impl Operated<'_> {
    /// The header again through the upgrade's reader: `20` when it is gone
    /// or refused, `28` when it is not the baseline's.
    fn view<H: Host>(&self, host: &mut H) -> Result<UpgradeView, Failure> {
        let view = match host.upgrade_view() {
            Some(Ok(view)) => view,
            None | Some(Err(_)) => return Err(Failure::Volume),
        };
        match self.read {
            Some(Ok(header)) if *header == view.header => Ok(view),
            _ => Err(Failure::Changed),
        }
    }

    /// The commit round's invitation once `margin` remains, and its
    /// acknowledgement for `description`, the operation's last step.
    fn commit_round(
        wire: &mut Wire,
        description: &[u8],
        margin: Duration,
        entropy: &mut dyn FnMut() -> Result<[u8; 32], String>,
    ) -> Result<(), Failure> {
        if remaining(wire.deadline()).map_err(|_| Failure::Timeout)? < margin {
            return Err(Failure::Timeout);
        }
        let round = entropy().map_err(|_| Failure::Internal)?;
        wire.exchange(COMMIT, COMMITTED, round, description, None)
            .map_err(|_| framing(wire))
    }

    /// Runs `steps`, a confirmed plan's, as key-less header commits.
    fn commit<H: Host>(host: &mut H, steps: &[Step], deadline: Instant) -> Result<(), Failure> {
        for step in steps {
            host.commit(step, deadline).map_err(|_| Failure::Failed)?;
        }
        Ok(())
    }

    /// Phase 1 (DESIGN.md "Storage operation worker", Upgrade): every read
    /// before the review, then each new key's ceremony, then after root's
    /// `13` the re-reads and the writes: the orphans' removals, the
    /// selector in its direct form, each new key's pending token and last
    /// `td-upgrade`.
    #[allow(clippy::too_many_arguments)]
    fn upgrade<H: Host, K: Keys>(
        &self,
        wire: &mut Wire,
        host: &mut H,
        keys: &mut K,
        started: Instant,
        margin: Duration,
        prepare: Prepare<'_>,
        entropy: &mut dyn FnMut() -> Result<[u8; 32], String>,
    ) -> Result<(), Failure> {
        let (shape, count, id) = match (self.request.operation(), self.baseline.current) {
            (
                Operation::Upgrade {
                    shape,
                    count,
                    current,
                    step: UpgradeStep::Review,
                    ..
                },
                Some(id),
            ) if self.baseline.volume == Volume::DeviceBound && *current == hex(&id) => {
                (*shape, *count, id)
            }
            _ => return Err(Failure::Internal),
        };
        self.request
            .upgrade_start()
            .map_err(|_| Failure::Internal)?;
        wire.limit(
            started
                .checked_add(upgrade_ceiling(count))
                .ok_or(Failure::Internal)?,
        );
        let (shape, role) = match shape {
            UpgradeShape::Tpm => (Shape::Tpm, FidoRole::Recovery),
            UpgradeShape::Fido2 => (Shape::Fido2, FidoRole::Primary),
        };
        // Every read before the review.
        let view = self.view(host)?;
        upgrade::phase_one_fits(&view, shape, usize::from(count)).map_err(|_| Failure::State)?;
        if shape == Shape::Tpm {
            host.tpm().map_err(Failure::Tpm)?;
        }
        let pair = Pair::read(host, &id, true, false, prepare)?;
        let round = entropy().map_err(|_| Failure::Internal)?;
        let bound = wire.bound();
        wire.exchange(
            PRESENT,
            PRESENTED,
            round,
            &self.request.encode(),
            Some(bound),
        )
        .map_err(|_| framing(wire))?;
        // The ceremonies: none of a header token naming a keyslot, nor an
        // earlier key's, may be created again.
        let named: Vec<&[u8]> = fido_tokens(&view.header)
            .into_iter()
            .filter(|token| token.keyslot.is_some())
            .map(|token| token.credential.as_slice())
            .collect();
        let volume = volume_uuid(&view.header.uuid).ok_or(Failure::Internal)?;
        let fingerprints: Vec<[u8; 4]> = self.baseline.tokens.iter().map(|(_, key)| *key).collect();
        let mut ceremony = Ceremony {
            wire,
            current: self.request.clone(),
            baseline: &fingerprints,
            key: 0,
            entropy,
            reported: None,
            cause: None,
        };
        let mut made: Vec<Fido2> = Vec::with_capacity(usize::from(count));
        for key in 1..=count {
            ceremony.key = key;
            ceremony.present(LoginStep::Connect, None)?;
            keys.connect(ceremony.wire.deadline())?;
            let mut excluded = named.clone();
            excluded.extend(made.iter().map(|key| key.credential.as_slice()));
            // One session's lifetime at most, inside the operation's.
            let deadline = Instant::now()
                .checked_add(MAX_LIFETIME)
                .map_or(ceremony.wire.deadline(), |lifetime| {
                    lifetime.min(ceremony.wire.deadline())
                });
            ceremony.wire.session(Some(deadline));
            let result = keys.create(deadline, &excluded, &volume, &mut ceremony);
            let created = result.map_err(|error| match error {
                // Only a creation's exclusion list names a credential.
                DiskError::Failed(FidoError::Status(Status::CredentialExcluded)) => {
                    Failure::Excluded
                }
                error => disk_failure(
                    error,
                    ceremony.cause.take(),
                    ceremony.reported.take(),
                    ceremony.wire,
                ),
            });
            ceremony.wire.session(None);
            let created = created?;
            let cose = Es256PublicKey::from_cose(&created.cose).map_err(|_| Failure::Failed)?;
            let (x, y) = cose.coordinates();
            // The passphrase is zeroed unused: the upgrade boot derives it.
            made.push(Fido2 {
                keyslot: None,
                role,
                credential: created.credential.clone(),
                salt: created.salt,
                x: *x,
                y: *y,
            });
        }
        let Ceremony {
            wire,
            current,
            entropy,
            ..
        } = ceremony;
        Self::commit_round(wire, &current.encode(), margin, entropy)?;
        // Root may now count a write as begun; nothing is written until
        // every read below agrees with what was presented.
        if host.header() != *self.read || host.standing() != self.baseline.current {
            return Err(Failure::Changed);
        }
        let mut esp = pair.reopen(host)?;
        let view = self.view(host).map_err(|_| Failure::Changed)?;
        let plan = upgrade::phase_one(&view, shape, &made).map_err(|_| Failure::Changed)?;
        let metadata = host.metadata().map_err(|_| Failure::Failed)?;
        let steps = plan.confirm(&metadata).map_err(|_| Failure::Changed)?;
        // `phase_one` puts every removal first: the selector is written
        // between them and the imports.
        let removed = steps
            .iter()
            .take_while(|step| matches!(step, Step::RemoveToken { .. }))
            .count();
        let (removals, imports) = steps.split_at_checked(removed).ok_or(Failure::Internal)?;
        if imports
            .iter()
            .any(|step| matches!(step, Step::RemoveToken { .. }))
        {
            return Err(Failure::Internal);
        }
        Self::commit(host, removals, wire.deadline())?;
        pair.write(&mut esp)?;
        drop(esp);
        Self::commit(host, imports, wire.deadline())?;
        wire.send(&[SUCCESS]).map_err(|_| Failure::Internal)
    }

    /// The cancellation in U0 (DESIGN.md "Storage operation worker",
    /// Cancellation): the plan before its one presentation, the header
    /// re-read after root's `13`, then the plan's removals.
    fn cancel<H: Host>(
        &self,
        wire: &mut Wire,
        host: &mut H,
        started: Instant,
        margin: Duration,
        entropy: &mut dyn FnMut() -> Result<[u8; 32], String>,
    ) -> Result<(), Failure> {
        if self.baseline.volume != Volume::Upgrading {
            return Err(Failure::Internal);
        }
        wire.limit(
            started
                .checked_add(UPGRADE_CANCEL_CEILING)
                .ok_or(Failure::Internal)?,
        );
        let view = self.view(host)?;
        let plan = upgrade::cancel(&view).map_err(|_| Failure::State)?;
        let description = self.request.encode();
        let round = entropy().map_err(|_| Failure::Internal)?;
        let bound = wire.bound();
        wire.exchange(PRESENT, PRESENTED, round, &description, Some(bound))
            .map_err(|_| framing(wire))?;
        Self::commit_round(wire, &description, margin, entropy)?;
        if host.header() != *self.read {
            return Err(Failure::Changed);
        }
        let metadata = host.metadata().map_err(|_| Failure::Failed)?;
        let steps = plan.confirm(&metadata).map_err(|_| Failure::Changed)?;
        Self::commit(host, &steps, wire.deadline())?;
        wire.send(&[SUCCESS]).map_err(|_| Failure::Internal)
    }
}

/// An upgrade's key ceremony, presented one step at a time: the connect
/// step, then td-fido's create, prove, repeat and probe steps, each once
/// the key has answered what it names, and the PIN root sends for each
/// PIN step.
pub(crate) struct Ceremony<'a> {
    wire: &'a mut Wire,
    /// The step last presented, which the next must succeed.
    current: Request,
    /// The header's fido2 tokens' fingerprints, which root admits against.
    baseline: &'a [[u8; 4]],
    /// The new key this ceremony creates, 1 or 2.
    key: u8,
    entropy: &'a mut dyn FnMut() -> Result<[u8; 32], String>,
    /// The retries of the PIN step presented, once its PIN was taken.
    reported: Option<u8>,
    /// Why a step failed, which td-fido reports only as an ended step.
    cause: Option<Failure>,
}

impl Ceremony<'_> {
    /// Presents `step` of this key as root admits it, `created` the new
    /// credential's fingerprint from prove on, and returns its encoding.
    fn present(&mut self, step: LoginStep, created: Option<[u8; 4]>) -> Result<Vec<u8>, Failure> {
        let Operation::Upgrade {
            account,
            shape,
            count,
            current,
            ..
        } = self.current.operation().clone()
        else {
            return Err(Failure::Internal);
        };
        let next = Request::new(
            *self.current.nonce(),
            self.current.owner(),
            Operation::Upgrade {
                account,
                shape,
                count,
                current,
                step: UpgradeStep::Key {
                    key: self.key,
                    step,
                },
                approval: None,
            },
        )
        .map_err(|_| Failure::Internal)?;
        // Exactly what root admits; td-fido never asks for a zero count.
        self.current
            .admit_upgrade_step(&next, self.baseline, created)
            .map_err(|_| Failure::Internal)?;
        let description = next.encode();
        let round = (self.entropy)().map_err(|_| Failure::Internal)?;
        self.wire
            .exchange(PRESENT, PRESENTED, round, &description, None)
            .map_err(|_| framing(self.wire))?;
        self.current = next;
        Ok(description)
    }

    fn pin_step(&mut self, step: CreateStep<'_>, retries: u8) -> Result<(Pin, Vec<u8>), Failure> {
        let created = step.credential().map(fingerprint);
        let login = match (step, created) {
            (CreateStep::Create, _) => LoginStep::Create { retries },
            (CreateStep::Prove(_), Some(key)) => LoginStep::Prove { key, retries },
            (CreateStep::Repeat(_), Some(key)) => LoginStep::Repeat { key, retries },
            _ => return Err(Failure::Internal),
        };
        let description = self.present(login, created)?;
        let mut secret = self.wire.receive_secret().map_err(|_| framing(self.wire))?;
        let pin = Pin::new(std::mem::take(&mut secret.0)).map_err(|_| Failure::Internal)?;
        Ok((pin, description))
    }
}

impl CreateSteps for Ceremony<'_> {
    fn pin(&mut self, step: CreateStep<'_>, retries: u8) -> Result<(Pin, Vec<u8>), String> {
        match self.pin_step(step, retries) {
            Ok(answer) => {
                self.reported = Some(retries);
                Ok(answer)
            }
            Err(failure) => {
                self.cause = Some(failure);
                Err("upgrade PIN step failed".into())
            }
        }
    }

    fn probe(&mut self, credential: &[u8]) -> Result<Vec<u8>, String> {
        let key = fingerprint(credential);
        self.present(LoginStep::Probe { key }, Some(key))
            .map_err(|failure| {
                self.cause = Some(failure);
                "upgrade probe step failed".to_string()
            })
    }

    /// The step's rows already ask for the touch; the wire has no frame.
    fn touch(&mut self, _: td_fido::fido_disk::Phase) {}
}

/// A disk proof: root's first description at the proof's first step, the
/// baseline it was built from, and the header read that baseline holds.
struct Proved<'a> {
    request: &'a Request,
    baseline: &'a Baseline,
    read: &'a Read,
}

impl Proved<'_> {
    /// Presents the proof's first step, takes its secret, and tests the
    /// keyslot it opens; then `14`. Nothing is written, so there is no
    /// commit round.
    fn run<H: Host, K: Keys>(
        &self,
        wire: &mut Wire,
        host: &mut H,
        keys: &mut K,
        started: Instant,
        entropy: &mut dyn FnMut() -> Result<[u8; 32], String>,
    ) -> Result<(), Failure> {
        let Some(Ok(header)) = self.read else {
            return Err(Failure::Internal);
        };
        // What root admitted against the same baseline.
        let tokens: Vec<[u8; 4]> = self.baseline.tokens.iter().map(|(_, key)| *key).collect();
        self.request
            .proof_start(DiskProofs {
                recovery_key: self.baseline.recovery,
                tokens: &tokens,
            })
            .map_err(|_| Failure::Internal)?;
        let ceiling = self.request.login_ceiling().ok_or(Failure::Internal)?;
        wire.limit(started.checked_add(ceiling).ok_or(Failure::Internal)?);
        // A disclosure's approval key is typed before root acknowledges.
        let wait = self.request.login_approval().map(|_| wire.bound());
        let round = entropy().map_err(|_| Failure::Internal)?;
        wire.exchange(PRESENT, PRESENTED, round, &self.request.encode(), wait)
            .map_err(|_| framing(wire))?;
        match self.request.login_proof() {
            Some(Proof::RecoveryKey) => self.recovery_key(wire, host, header),
            Some(Proof::DiskKey) => self.disk_key(wire, host, keys, header, &tokens, entropy),
            None => Err(Failure::Internal),
        }?;
        wire.send(&[SUCCESS]).map_err(|_| Failure::Internal)
    }

    /// The typed key, parsed by td-protector's codec and tested on the
    /// keyslot the `recovery-key` marker names. The entry, the key and its
    /// passphrase each live in storage zeroed on drop, and reach
    /// cryptsetup only through its standard-input pipe.
    fn recovery_key<H: Host>(
        &self,
        wire: &mut Wire,
        host: &mut H,
        header: &Header,
    ) -> Result<(), Failure> {
        let (_, slot) = protected_plan::marker(header).ok_or(Failure::Internal)?;
        let entry = wire.receive_secret().map_err(|_| framing(wire))?;
        let key = RecoveryKey::parse(entry.bytes());
        drop(entry);
        let passphrase = key.map_err(|_| Failure::Mistyped)?.passphrase();
        self.unchanged(host)?;
        tested(host.test_keyslot(slot, passphrase.expose(), header, wire.deadline()))
    }

    /// The disk key's two presented steps through td-fido, its verified
    /// assertion's derived passphrase tested on the keyslot its token
    /// names.
    fn disk_key<H: Host, K: Keys>(
        &self,
        wire: &mut Wire,
        host: &mut H,
        keys: &mut K,
        header: &Header,
        fingerprints: &[[u8; 4]],
        entropy: &mut dyn FnMut() -> Result<[u8; 32], String>,
    ) -> Result<(), Failure> {
        // A token whose point td-fido refuses is not tried, as the
        // selector's release skips it: the rest still prove.
        let mut fido = Vec::with_capacity(luks2::MAX_FIDO2_TOKENS);
        let mut tokens = Vec::with_capacity(luks2::MAX_FIDO2_TOKENS);
        let mut usable = Vec::with_capacity(luks2::MAX_FIDO2_TOKENS);
        for (token, fingerprint) in fido_tokens(header).into_iter().zip(fingerprints) {
            if let Ok(key) = PublicKey::from_coordinates(&token.x, &token.y) {
                tokens.push(fido_disk::Token {
                    credential: &token.credential,
                    key,
                    salt: token.salt,
                });
                fido.push(token);
                usable.push(*fingerprint);
            }
        }
        let volume = volume_uuid(&header.uuid).ok_or(Failure::Internal)?;
        // One session's lifetime at most, inside the operation's.
        let deadline = Instant::now()
            .checked_add(MAX_LIFETIME)
            .map_or(wire.deadline(), |lifetime| lifetime.min(wire.deadline()));
        wire.session(Some(deadline));
        let identify = self.request.encode();
        let mut presenter = Presenter {
            wire,
            request: self.request,
            fingerprints: &usable,
            entropy,
            reported: None,
            cause: None,
        };
        let result = keys.authorize(deadline, &tokens, &volume, &identify, &mut presenter);
        let Presenter {
            wire,
            reported,
            cause,
            ..
        } = presenter;
        // Classified while the session still bounds the wire.
        let released = result.map_err(|error| disk_failure(error, cause, reported, wire));
        wire.session(None);
        let released = released?;
        let slot = fido
            .get(released.token)
            .and_then(|token| token.keyslot)
            .ok_or(Failure::WrongKey)?;
        self.unchanged(host)?;
        tested(host.test_keyslot(slot, released.passphrase.bytes(), header, wire.deadline()))
    }

    /// The header as the baseline read it: `20` when it is gone or now
    /// refused, `28` when it changed, so the keyslot test is of the
    /// protectors root admitted the proof against.
    fn unchanged<H: Host>(&self, host: &mut H) -> Result<(), Failure> {
        let now = host.header();
        match &now {
            None | Some(Err(_)) => Err(Failure::Volume),
            _ if now != *self.read => Err(Failure::Changed),
            _ => Ok(()),
        }
    }
}

/// The header's fido2 tokens in the baseline's order: token-number order,
/// at most four.
fn fido_tokens(header: &Header) -> Vec<&Fido2> {
    header
        .protected
        .iter()
        .filter_map(|(_, token)| match token {
            Token::Fido2(fido) => Some(fido),
            _ => None,
        })
        .take(luks2::MAX_FIDO2_TOKENS)
        .collect()
}

/// A keyslot test's answer.
fn tested(result: Result<Tested, String>) -> Result<(), Failure> {
    match result {
        Ok(Tested::Opened) => Ok(()),
        Ok(Tested::Refused) => Err(Failure::WrongKey),
        Ok(Tested::Changed) => Err(Failure::Changed),
        Ok(Tested::Expired) => Err(Failure::Timeout),
        Err(_) => Err(Failure::Failed),
    }
}

/// The disk authorize step, presented once the key has said its retries,
/// and the PIN root sends for it.
pub(crate) struct Presenter<'a> {
    wire: &'a mut Wire,
    /// The proof's first step, disk identify.
    request: &'a Request,
    fingerprints: &'a [[u8; 4]],
    entropy: &'a mut dyn FnMut() -> Result<[u8; 32], String>,
    /// The retries of the step presented, once its PIN was taken.
    reported: Option<u8>,
    /// Why the step failed, which td-fido reports only as an ended PIN.
    cause: Option<Failure>,
}

impl Presenter<'_> {
    fn present(&mut self, token: usize, retries: u8) -> Result<(Pin, Vec<u8>), Failure> {
        let key = *self.fingerprints.get(token).ok_or(Failure::Internal)?;
        let next = stepped(self.request, LoginStep::DiskAuthorize { key, retries })?;
        // Exactly what root admits; td-fido never asks for a zero count.
        if self.request.login_successor(&next).is_err() {
            return Err(Failure::Internal);
        }
        let description = next.encode();
        let round = (self.entropy)().map_err(|_| Failure::Internal)?;
        self.wire
            .exchange(PRESENT, PRESENTED, round, &description, None)
            .map_err(|_| framing(self.wire))?;
        let mut secret = self.wire.receive_secret().map_err(|_| framing(self.wire))?;
        let pin = Pin::new(std::mem::take(&mut secret.0)).map_err(|_| Failure::Internal)?;
        Ok((pin, description))
    }
}

impl Steps for Presenter<'_> {
    fn authorize(&mut self, token: usize, retries: u8) -> Result<(Pin, Vec<u8>), String> {
        match self.present(token, retries) {
            Ok(answer) => {
                self.reported = Some(retries);
                Ok(answer)
            }
            Err(failure) => {
                self.cause = Some(failure);
                Err("disk authorize step failed".into())
            }
        }
    }
}

/// `request`'s operation at `step`: a proof's later step carries no
/// disclosure.
fn stepped(request: &Request, step: LoginStep) -> Result<Request, Failure> {
    let operation = match request.operation().clone() {
        Operation::LoginEnroll {
            account,
            before,
            after,
            key,
            proof,
            ..
        } => Operation::LoginEnroll {
            account,
            before,
            after,
            key,
            step,
            approval: None,
            proof,
        },
        Operation::LoginAdd {
            account,
            before,
            after,
            proof,
            ..
        } => Operation::LoginAdd {
            account,
            before,
            after,
            step,
            proof,
        },
        Operation::LoginRemove {
            account,
            before,
            after,
            removed,
            proof,
            ..
        } => Operation::LoginRemove {
            account,
            before,
            after,
            removed,
            step,
            approval: None,
            proof,
        },
        _ => return Err(Failure::Internal),
    };
    Request::new(*request.nonce(), request.owner(), operation).map_err(|_| Failure::Internal)
}

/// td-fido's typed refusals and statuses select the kind, never text; a
/// wrong PIN is one only after this proof asked for it, and a failed step
/// is its own cause.
fn disk_failure(
    error: DiskError,
    cause: Option<Failure>,
    reported: Option<u8>,
    wire: &Wire,
) -> Failure {
    match error {
        DiskError::OneKey => Failure::OneKey,
        DiskError::NotEnrolled => Failure::NotEnrolled,
        DiskError::Refused(refusal) => Failure::Refused(refusal),
        DiskError::Ended => cause.unwrap_or(Failure::Internal),
        DiskError::Unselectable => Failure::Unprobed,
        DiskError::CredentialLength | DiskError::Mismatch => Failure::Failed,
        DiskError::Failed(error) => match error {
            FidoError::Status(Status::PinInvalid) => {
                reported.map_or(Failure::Failed, Failure::WrongPin)
            }
            FidoError::Status(Status::PinAuthBlocked) => Failure::PinAuthBlocked,
            FidoError::Status(Status::PinBlocked) => Failure::PinBlocked,
            FidoError::Status(Status::Denied) => Failure::Denied,
            FidoError::Status(Status::TouchTimeout | Status::ActionTimeout)
            | FidoError::Interrupted(Interruption::Expired) => Failure::Timeout,
            FidoError::Entropy | FidoError::PinInput => Failure::Internal,
            FidoError::Status(_)
            | FidoError::Interrupted(_)
            | FidoError::Transport
            | FidoError::Protocol(_) => expired_or(wire, Failure::Failed),
        },
    }
}

/// A deadline that has passed, an open session's included, is a timeout.
fn expired_or(wire: &Wire, failure: Failure) -> Failure {
    if remaining(wire.bound()).is_err() {
        Failure::Timeout
    } else {
        failure
    }
}

/// A channel failure: TIMEOUT when a deadline ran out, else INTERNAL.
fn framing(wire: &Wire) -> Failure {
    if wire.expired() {
        Failure::Timeout
    } else {
        Failure::Internal
    }
}

/// The verb: startup's checks, the memory state, discovery, then the
/// operation. Its exit is success for `14` and for a definite `28`.
pub(crate) fn run(uid: u32) -> Result<(), String> {
    let stream = startup()?;
    // Root's end closing is how cancellation reaches a keyslot test's
    // child, which the endpoint does not read for.
    let authority = stream.try_clone().map_err(|_| "clone operation endpoint")?;
    let started = Instant::now();
    // The longest operation's until the description names one.
    let deadline = started
        .checked_add(SELECTOR_UPDATE_CEILING.max(LOGIN_LONGEST))
        .ok_or("storage operation deadline overflow")?;
    let mut wire = Wire::new(stream, deadline)?;
    // Before the baseline, as td-secret's workers do: a disk proof holds a
    // recovery key, a PIN or a derived passphrase.
    let memory = crate::memory::with_memory_state(
        paths::read_to_string(Path::new("/proc/swaps")),
        paths::read_to_string(Path::new("/proc/self/limits")),
        || paths::open_read(Path::new("/dev/urandom")).map_err(|e| e.to_string()),
    );
    let mut random = match memory {
        Ok(random) => random,
        Err(error) => {
            let _ = wire.send(&[FAILURE, Failure::Internal.kind()]);
            return Err(format!("storage operation refused its start: {error}"));
        }
    };
    let mut host = Machine::discover();
    host.authority = Some(authority);
    sweep(&host.root.join(RUN_DIR), ROOT_UID, &|pid| {
        paths::metadata_if_present(Path::new(&format!("/proc/{pid}"))).is_some()
    });
    let mut keys = match paths::open_read(Path::new("/dev/urandom")) {
        Ok(random) => RootKeys {
            random,
            previous: None,
        },
        Err(_) => {
            let _ = wire.send(&Failure::Internal.frame());
            return Err("storage operation refused its start: no kernel entropy".into());
        }
    };
    operate(
        &mut wire,
        &mut host,
        &mut keys,
        uid,
        started,
        COMMIT_MARGIN,
        &prepare,
        &mut || {
            let mut round = [0; 32];
            random
                .read_exact(&mut round)
                .map_err(|_| "read kernel entropy".to_string())?;
            Ok(round)
        },
    )
    .map_err(|failure| format!("storage operation failed: {failure:?}"))
}

/// What the worker's process files say, read through this crate's path
/// rules, checked by the shared endpoint rules.
fn startup() -> Result<UnixStream, String> {
    let status = paths::read_bounded(Path::new("/proc/self/status"), 8192)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or("read operation process identity")?;
    let metadata = |fd: u8| {
        paths::metadata(Path::new(&format!("/proc/self/fd/{fd}")))
            .map_err(|_| "missing operation descriptor".to_string())
    };
    let descriptors = [metadata(0)?, metadata(1)?, metadata(2)?];
    let open = paths::read_dir_bounded(Path::new("/proc/self/fd"), 5)
        .map_err(|_| "inspect operation descriptors")?
        .iter()
        .filter_map(|path| path.file_name().map(std::ffi::OsStr::to_os_string))
        .collect();
    frames::endpoint(frames::Process {
        status,
        descriptors,
        open,
    })
}

/// The verb that serves a disk proof's HID sessions: td-fido's root worker
/// under this program, as td-boot's selector serves its own.
pub(crate) const FIDO_WORKER: &str = "fido-worker";

/// td-install as its own HID worker. The worker discovers root-admitted
/// nodes only, so the desktop verb is never built into a worker's argv,
/// and td-install does not dispatch it.
pub(crate) struct Worker;
impl Program for Worker {
    const PATH: &'static str = "/proc/self/exe";
    const ROOT: &'static str = FIDO_WORKER;
    const DESKTOP: &'static str = "fido-worker-desktop";
}

/// The root USB transport: root-owned mode-0600 `/dev/hidrawN` nodes.
struct Root;
impl Devices for Root {
    type Node = fido_device::Device;
    type Channel = fido_device::Session<Worker>;
    fn discover(&mut self) -> Result<Vec<Self::Node>, String> {
        fido_device::Device::discover()
    }
    fn open(&mut self, node: Self::Node, deadline: Instant) -> Result<Self::Channel, String> {
        fido_device::Session::open(node, deadline)
    }
    fn pause(&mut self, time: Duration) {
        std::thread::sleep(time);
    }
}

/// Production's disk security keys, td-fido's draws from the kernel.
struct RootKeys {
    random: File,
    /// The key the last connect step found, which the next must not be.
    previous: Option<fido_device::Device>,
}

/// How often a connect step looks for the new key.
const POLL: Duration = Duration::from_millis(100);

impl Keys for RootKeys {
    fn authorize(
        &mut self,
        deadline: Instant,
        tokens: &[fido_disk::Token<'_>],
        volume: &[u8; 16],
        identify: &[u8],
        steps: &mut Presenter<'_>,
    ) -> Result<Released, DiskError> {
        let random = &mut self.random;
        fido_disk::authorize(
            &mut Root,
            deadline,
            tokens,
            volume,
            identify,
            steps,
            &mut |out| {
                random
                    .read_exact(out)
                    .map_err(|_| "read kernel entropy".into())
            },
        )
    }

    fn connect(&mut self, deadline: Instant) -> Result<(), Failure> {
        loop {
            let left = remaining(deadline).map_err(|_| Failure::Timeout)?;
            match Root.discover().map_err(|_| Failure::Failed)?.as_slice() {
                [node] if Some(*node) != self.previous => {
                    self.previous = Some(*node);
                    return Ok(());
                }
                [] | [_] => Root.pause(POLL.min(left)),
                _ => return Err(Failure::OneKey),
            }
        }
    }

    fn create(
        &mut self,
        deadline: Instant,
        excluded: &[&[u8]],
        volume: &[u8; 16],
        steps: &mut Ceremony<'_>,
    ) -> Result<Created, DiskError> {
        let random = &mut self.random;
        fido_disk::create_stepped(&mut Root, deadline, excluded, volume, steps, &mut |out| {
            random
                .read_exact(out)
                .map_err(|_| "read kernel entropy".into())
        })
    }
}

/// `prepare-selector`'s rules in memory: the template, its padding and the
/// appended identity, with the measurement policy on a protected volume.
fn prepare(template: &[u8], key: &[u8], uuid: &str, measured: bool) -> Result<Vec<u8>, String> {
    let uuid = crate::VolumeUuid::parse(uuid).map_err(|e| e.to_string())?;
    crate::selector_bytes(template, key, &uuid, measured).map_err(|e| e.to_string())
}

/// Where the running system holds the volume, and what it booted, under
/// the machine's root.
const VOLUME_MOUNT: &str = "run/td-volume";
const DEPLOYMENT: &str = "run/td-deployment";
const CMDLINE: &str = "/proc/cmdline";
const TD_BOOT: &str = "/bin/td-boot";
const MAX_CMDLINE_BYTES: usize = 4096;

/// The running machine: the volume `td.volume=` names, as td-boot's
/// discovery finds it, its LUKS2 partition held for the operation, and the
/// disk that partition is on.
pub(crate) struct Machine {
    uuid: String,
    cmdline: String,
    partition: Option<File>,
    /// The held partition's device node, which a keyslot test names.
    device: Option<PathBuf>,
    /// A second handle on root's socket, which a keyslot test peeks for
    /// root hanging up; none in host tests that do not give one.
    authority: Option<UnixStream>,
    /// `/`; a scratch directory in host tests.
    root: PathBuf,
}

impl Machine {
    /// Discovery through `td-boot volume`, td-boot's own scan, whose line
    /// names the UUID, the partition and, for an encrypted volume, its
    /// active `td-system` mapping. Anything else leaves no held partition,
    /// which the baseline reads as no encrypted td volume.
    pub(crate) fn discover() -> Self {
        let cmdline = paths::read_bounded(Path::new(CMDLINE), MAX_CMDLINE_BYTES)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default();
        let mut uuids = cmdline
            .split_ascii_whitespace()
            .filter_map(|token| token.strip_prefix(protocol::VOLUME_CMDLINE_PREFIX));
        let uuid = match (uuids.next(), uuids.next()) {
            (Some(uuid), None) => uuid.to_owned(),
            _ => String::new(),
        };
        let (device, partition) = (!uuid.is_empty())
            .then(|| volume_partition(&uuid))
            .flatten()
            .unzip();
        Self {
            uuid,
            cmdline,
            partition,
            device,
            authority: None,
            root: PathBuf::from("/"),
        }
    }

    fn volume(&self) -> PathBuf {
        self.root.join(VOLUME_MOUNT)
    }
}

/// The LUKS2 partition `td-boot volume UUID` names, and its node opened,
/// when its line is exactly the UUID, a device and a mapping node.
fn volume_partition(uuid: &str) -> Option<(PathBuf, File)> {
    let output = Command::new(TD_BOOT)
        .args(["volume", uuid])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = std::str::from_utf8(&output.stdout)
        .ok()?
        .strip_suffix('\n')?;
    match line.split(' ').collect::<Vec<_>>().as_slice() {
        [found, device, mapping]
            if *found == uuid && device.starts_with("/dev/") && mapping.starts_with("/dev/") =>
        {
            let device = PathBuf::from(device);
            let partition = paths::open_read(&device).ok()?;
            Some((device, partition))
        }
        _ => None,
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = sha256::Sha256::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// The 64 lowercase hexadecimal digits of `text` as bytes.
fn id_bytes(text: &[u8]) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let (pairs, []) = text.as_chunks::<2>() else {
        return None;
    };
    let mut id = [0u8; 32];
    for (slot, [high, low]) in id.iter_mut().zip(pairs) {
        *slot = digit(*high)? << 4 | digit(*low)?;
    }
    Some(id)
}

/// The kernel of the deployment td-boot authenticate's line names: that
/// line is the ID's, the manifest hashes to the ID, and `bzImage` is the
/// file the manifest names, hashed here.
fn authenticated_kernel(
    directory: &Path,
    id: &[u8; 32],
    authenticated: &[u8],
) -> Result<Vec<u8>, String> {
    if authenticated != format!("{}\n", hex(id)).as_bytes() {
        return Err("current's deployment does not authenticate".into());
    }
    let manifest = paths::read_bounded(
        &directory.join(protocol::MANIFEST_NAME),
        protocol::MAX_MANIFEST_BYTES as usize,
    )
    .map_err(|e| e.to_string())?;
    if digest(&manifest) != *id {
        return Err("current's manifest is not its ID".into());
    }
    let expected = manifest_digest(&manifest, "bzImage").ok_or("the manifest names no bzImage")?;
    let kernel = td_fs::read_bounded_real_file(
        &directory.join("bzImage"),
        "deployment kernel",
        MAX_BOOT_FILE,
    )
    .map_err(|e| e.to_string())?;
    if digest(&kernel) != expected {
        return Err("bzImage does not match current's manifest".into());
    }
    Ok(kernel)
}

/// The SHA-256 the manifest gives `name`: its line `<64 hex>  <name>`.
fn manifest_digest(manifest: &[u8], name: &str) -> Option<[u8; 32]> {
    let text = std::str::from_utf8(manifest).ok()?;
    let mut lines = text.split_terminator('\n');
    if lines.next()?.as_bytes() != protocol::MANIFEST_HEADER {
        return None;
    }
    let mut found = None;
    for line in lines {
        let (digest, entry) = line.split_once("  ")?;
        if entry == name {
            if found.is_some() {
                return None;
            }
            found = Some(id_bytes(digest.as_bytes())?);
        }
    }
    found
}

impl Host for Machine {
    type Disk = File;

    fn header(&mut self) -> Read {
        let partition = self.partition.as_mut()?;
        Some(luks2::read(partition).and_then(|header| {
            if header.uuid == self.uuid {
                Ok(header)
            } else {
                Err("the held partition's header names another volume".into())
            }
        }))
    }

    fn uuid(&self) -> &str {
        &self.uuid
    }

    /// The deployment the selector booted (`/run/td-deployment`) when the
    /// volume's `current` names it, no attempt is pending for it and the
    /// selector could keep its bookkeeping.
    fn standing(&mut self) -> Option<[u8; 32]> {
        let running = paths::read_bounded(&self.root.join(DEPLOYMENT), 80).ok()?;
        let id = id_bytes(running.strip_suffix(b"\n")?)?;
        let name = hex(&id);
        let volume = self.volume();
        let current =
            paths::read_link(&volume.join(protocol::BOOT_DIR).join(protocol::CURRENT_SLOT)).ok()?;
        if current != Path::new(&format!("{}{name}", protocol::SELECTOR_PREFIX)) {
            return None;
        }
        match paths::symlink_metadata(&volume.join(protocol::ATTEMPTS_DIR).join(&name)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return None,
        }
        let bookkeeping = self
            .cmdline
            .split_ascii_whitespace()
            .any(|token| token == protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN);
        (!bookkeeping).then_some(id)
    }

    /// `td-boot authenticate` checks the deployment's manifest and
    /// signature under the volume's trusted key: a bounded read and no
    /// payload hash, so a worker root kills leaves no long child behind.
    fn kernel(&mut self, id: &[u8; 32]) -> Result<Vec<u8>, String> {
        let volume = self.volume();
        let directory = volume.join(protocol::DEPLOYMENTS_DIR).join(hex(id));
        let output = Command::new(TD_BOOT)
            .arg("authenticate")
            .arg(&directory)
            .arg(volume.join(protocol::VOLUME_TRUSTED_KEY))
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("run td-boot authenticate: {e}"))?;
        if !output.status.success() {
            return Err("current's deployment does not authenticate".into());
        }
        authenticated_kernel(&directory, id, &output.stdout)
    }

    fn template(&mut self) -> Result<Vec<u8>, String> {
        td_fs::read_bounded_real_file(
            &self.root.join(protocol::SELECTOR_TEMPLATE_PATH),
            "selector template",
            MAX_BOOT_FILE,
        )
        .map_err(|e| e.to_string())
    }

    fn trusted_key(&mut self) -> Result<Vec<u8>, String> {
        read_trusted_key(&self.volume().join(protocol::VOLUME_TRUSTED_KEY))
            .map_err(|e| e.to_string())
    }

    /// The held partition's disk, read-write, and its `td-esp` partition,
    /// which td's layout makes the first.
    fn esp(&mut self) -> Result<td_fat::Esp<File>, String> {
        use std::os::linux::fs::MetadataExt;
        let partition = self.partition.as_ref().ok_or("no held partition")?;
        let rdev = partition.metadata().map_err(|e| e.to_string())?.st_rdev();
        let disk = inventory::backing_disk(rdev).map_err(|e| e.to_string())?;
        let mut file =
            paths::open_read_write(&Path::new("/dev").join(&disk)).map_err(|e| e.to_string())?;
        let sector = logical_sector_size(&file).map_err(|e| e.to_string())?;
        let bytes = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        let (offset, len) =
            esp_region(&mut file, sector, bytes / sector).map_err(|e| e.to_string())?;
        td_fat::Esp::open(file, offset, len).map_err(|e| e.to_string())
    }

    /// The installed cryptsetup against a detached copy of the held
    /// partition's header area, the node it names checked to be the held
    /// one; the child is killed and reaped at `deadline` or when root hangs
    /// up. Its errors name no key.
    fn test_keyslot(
        &mut self,
        slot: u8,
        key: &[u8],
        expected: &Header,
        deadline: Instant,
    ) -> Result<Tested, String> {
        let device = self.device.as_ref().ok_or("no held partition")?;
        let partition = self.partition.as_mut().ok_or("no held partition")?;
        let held = partition.metadata().map_err(|e| e.to_string())?;
        let named = paths::metadata(device).map_err(|e| e.to_string())?;
        if !same_node(&held, &named) {
            return Err("the partition's node no longer names the held partition".into());
        }
        let area = luks2::test_area(partition)?;
        if area.header != *expected {
            return Ok(Tested::Changed);
        }
        area.cost(slot)?;
        let copy = DetachedHeader::take(&self.root.join(RUN_DIR), partition, area.length)?;
        // The copy cryptsetup reads is the header presented, or nothing
        // runs: a disk that changed while it was copied fails here.
        let mut reread = paths::open_read(&copy.path).map_err(|e| e.to_string())?;
        if luks2::test_area(&mut reread).ok() != Some(area) {
            return Ok(Tested::Changed);
        }
        drop(reread);
        let program = Cryptsetup::new(self.root.join("bin").join(protocol::CRYPTSETUP));
        let authority = self.authority.as_ref();
        let mut abandoned = || authority.is_some_and(hung_up);
        let args = cryptsetup::detached_test_args(&copy.path, device, slot);
        match program.run_bounded(&args, key, deadline, &mut abandoned) {
            Ok(Bounded::Exited(status)) if status.success() => Ok(Tested::Opened),
            Ok(Bounded::Exited(status))
                if status.code() == Some(cryptsetup::EXIT_BAD_PASSPHRASE) =>
            {
                Ok(Tested::Refused)
            }
            Ok(Bounded::Exited(_)) => Err("cryptsetup's keyslot test failed".into()),
            Ok(Bounded::Expired) => Ok(Tested::Expired),
            Ok(Bounded::Abandoned) => Err("root hung up during the keyslot test".into()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn upgrade_view(&mut self) -> Option<Result<UpgradeView, String>> {
        let partition = self.partition.as_mut()?;
        Some(luks2::read_upgrade(partition).and_then(|view| {
            if view.header.uuid == self.uuid {
                Ok(view)
            } else {
                Err("the held partition's header names another volume".into())
            }
        }))
    }

    /// td-tpm over the resource manager: one PCR_Read of the selector's
    /// PCRs, which answers whether a SHA-256 bank exists, then
    /// `TPM_PT_PERMANENT`, both without authorization.
    fn tpm(&mut self) -> Result<(), TpmRefusal> {
        let transport = td_tpm::Device::open().map_err(|_| TpmRefusal::Absent)?;
        let mut client = td_tpm::Client::new(transport);
        match td_protector::release_policy(&mut client) {
            Ok(_) => {}
            Err(td_tpm::PcrReadError::NoSha256Bank) => return Err(TpmRefusal::NoSha256Bank),
            Err(td_tpm::PcrReadError::Other(_)) => return Err(TpmRefusal::Absent),
        }
        let state = client.dictionary_attack().map_err(|_| TpmRefusal::Absent)?;
        if state.lockout_auth_set {
            Err(TpmRefusal::LockoutSet)
        } else {
            Ok(())
        }
    }

    fn metadata(&mut self) -> Result<Vec<u8>, String> {
        let device = self.held_node()?;
        self.cryptsetup()
            .metadata(&device)
            .map_err(|e| e.to_string())
    }

    /// The installed cryptsetup's token removal or import on the held
    /// partition's node, checked to be the held one; its standard input
    /// carries an imported token's text and never a key.
    fn commit(&mut self, step: &Step, deadline: Instant) -> Result<(), String> {
        let device = self.held_node()?;
        let input = match step.input() {
            upgrade::Input::Token(token) => token.encode()?.into_bytes(),
            upgrade::Input::Nothing => Vec::new(),
            upgrade::Input::Key(_) => return Err("a header commit takes no key".into()),
        };
        let args = step
            .args(&device, "td-system", Path::new("/nonexistent"))
            .ok_or("not a cryptsetup header commit")?;
        match self
            .cryptsetup()
            .run_bounded(&args, &input, deadline, &mut || false)
        {
            Ok(Bounded::Exited(status)) if status.success() => Ok(()),
            Ok(_) => Err("cryptsetup's header commit failed".into()),
            Err(error) => Err(error.to_string()),
        }
    }
}

impl Machine {
    fn cryptsetup(&self) -> Cryptsetup {
        Cryptsetup::new(self.root.join("bin").join(protocol::CRYPTSETUP))
    }

    /// The node `td-boot volume` named, while it still names the held
    /// partition.
    fn held_node(&self) -> Result<PathBuf, String> {
        let device = self.device.as_ref().ok_or("no held partition")?;
        let partition = self.partition.as_ref().ok_or("no held partition")?;
        let held = partition.metadata().map_err(|e| e.to_string())?;
        let named = paths::metadata(device).map_err(|e| e.to_string())?;
        if !same_node(&held, &named) {
            return Err("the partition's node no longer names the held partition".into());
        }
        Ok(device.clone())
    }
}

/// Where a keyslot test's detached header copy lives: tmpfs, root's.
const RUN_DIR: &str = "run";
/// A copy's directory under `RUN_DIR`: this prefix and the worker's pid.
const COPY_PREFIX: &str = "td-keyslot-test.";
/// The most `RUN_DIR` entries a sweep reads; past it, it sweeps nothing.
const RUN_ENTRIES: usize = 4096;
const ROOT_UID: u32 = 0;

/// Removes the copies killed workers left: each a real directory, owned
/// by `owner` (root), named for a pid no live process has. A live
/// worker's, this one's included, and anything else under `run` stay.
/// Best effort: a copy holds no secret, and reboot empties tmpfs.
fn sweep(run: &Path, owner: u32, alive: &dyn Fn(u32) -> bool) {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(entries) = paths::read_dir_bounded(run, RUN_ENTRIES) else {
        return;
    };
    for entry in entries {
        let Some(pid) = entry
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(COPY_PREFIX))
            .and_then(|pid| pid.parse::<u32>().ok().filter(|n| n.to_string() == pid))
        else {
            continue;
        };
        let Ok(metadata) = paths::symlink_metadata(&entry) else {
            continue;
        };
        if metadata.is_dir() && metadata.uid() == owner && !alive(pid) {
            let _ = paths::remove_dir_all_if_present(&entry);
        }
    }
}

/// A private copy of a header area, in a directory of its own made for
/// it, mode 0700, and removed with it when dropped.
struct DetachedHeader {
    dir: PathBuf,
    path: PathBuf,
}

impl DetachedHeader {
    /// `length` bytes from the start of `partition`, the copies and the
    /// keyslots area, read through the held descriptor.
    fn take(run: &Path, partition: &mut File, length: u64) -> Result<Self, String> {
        let dir = run.join(format!("{COPY_PREFIX}{}", std::process::id()));
        // Only root writes there: a directory left by a killed worker
        // under this pid is its own.
        paths::remove_dir_all_if_present(&dir).map_err(|e| e.to_string())?;
        paths::create_dir_with_mode(&dir, 0o700).map_err(|e| e.to_string())?;
        let copy = Self {
            path: dir.join("header"),
            dir,
        };
        let mut file = paths::create_new_with_mode(&copy.path, 0o600).map_err(|e| e.to_string())?;
        partition
            .seek(SeekFrom::Start(0))
            .map_err(|e| e.to_string())?;
        let copied = io::copy(&mut partition.take(length), &mut file).map_err(|e| e.to_string())?;
        if copied != length {
            return Err("the held partition ends inside its header area".into());
        }
        Ok(copy)
    }
}

impl Drop for DetachedHeader {
    fn drop(&mut self) {
        let _ = paths::remove_dir_all_if_present(&self.dir);
    }
}

/// Whether `named` is the node `held` was opened from: the same block
/// device, or for anything else the same file.
fn same_node(held: &std::fs::Metadata, named: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let block = |metadata: &std::fs::Metadata| metadata.file_type().is_block_device();
    match (block(held), block(named)) {
        (true, true) => held.rdev() == named.rdev(),
        (false, false) => (held.dev(), held.ino()) == (named.dev(), named.ino()),
        _ => false,
    }
}

/// Whether root closed its end of the socket, or broke the protocol: root
/// sends nothing while a keyslot test runs, so any byte a read finds,
/// end of file included, abandons it. The endpoint sets its own read
/// timeout before each read, so this one's is never left in its way.
fn hung_up(mut authority: &UnixStream) -> bool {
    if authority
        .set_read_timeout(Some(Duration::from_millis(1)))
        .is_err()
    {
        return true;
    }
    let mut byte = [0; 1];
    match authority.read(&mut byte) {
        // End of file, or a byte root never sends during a test.
        Ok(count) => count <= byte.len(),
        Err(error) => !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]
    use super::*;
    use crate::consent::ApprovalKey;
    use std::cell::RefCell;
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;
    use std::rc::Rc;
    use td_protector::luks2::HeaderCopy;
    use td_protector::protected::Fido2;

    const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
    const ID: [u8; 32] = [0xab; 32];
    const NONCE: [u8; 32] = [42; 32];
    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
    /// The ESP's first sector on the test disk.
    const BASE: u64 = 1 << 20;
    const SECTORS: u64 = 67_000;

    /// A disk image in memory that every `esp()` call reopens.
    #[derive(Clone)]
    struct Disk(Rc<RefCell<Vec<u8>>>);

    impl td_fat::Device for Disk {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            let bytes = self.0.borrow();
            let at = usize::try_from(offset).unwrap();
            let slice = bytes
                .get(at..at + buf.len())
                .ok_or_else(|| std::io::Error::other("read past the disk"))?;
            buf.copy_from_slice(slice);
            Ok(())
        }
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
            let mut bytes = self.0.borrow_mut();
            let at = usize::try_from(offset).unwrap();
            bytes
                .get_mut(at..at + buf.len())
                .ok_or_else(|| std::io::Error::other("write past the disk"))?
                .copy_from_slice(buf);
            Ok(())
        }
        fn sync(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// td's layout's ESP holding `kernel` and `initrd` as the pair.
    fn esp_image(kernel: &[u8], initrd: &[u8], sectors: u64) -> Disk {
        use crate::fat::Node;
        let volume = crate::fat::Volume {
            bytes_per_sector: 512,
            total_sectors: sectors,
            hidden_sectors: (BASE / 512) as u32,
            volume_id: 0x7d1e_9a30,
            label: "TD-ESP".into(),
            sectors_per_cluster: Some(1),
            root: vec![(
                "EFI".into(),
                Node::Dir(vec![(
                    "BOOT".into(),
                    Node::Dir(vec![
                        ("BOOTX64.EFI".into(), Node::File(kernel)),
                        ("INITRD".into(), Node::File(initrd)),
                    ]),
                )]),
            )],
        };
        let image = crate::fat::build(&volume).unwrap();
        let mut bytes = vec![0u8; usize::try_from(BASE + sectors * 512).unwrap()];
        for extent in &image.extents {
            let at = usize::try_from(BASE + extent.offset).unwrap();
            bytes[at..at + extent.bytes.len()].copy_from_slice(&extent.bytes);
        }
        Disk(Rc::new(RefCell::new(bytes)))
    }

    fn header(protected: Vec<(u8, Token)>) -> Header {
        Header {
            copy: HeaderCopy::Primary,
            seqid: 1,
            hdr_size: 16384,
            uuid: UUID.into(),
            label: "td-system".into(),
            keyslots: vec![0, 1],
            tokens: Vec::new(),
            orphans: Vec::new(),
            foreign: Vec::new(),
            protected,
        }
    }

    fn fido(role: FidoRole, keyslot: u8, credential: u8) -> Token {
        Token::Fido2(Fido2 {
            keyslot: Some(keyslot),
            role,
            credential: vec![credential; 16],
            salt: [3; 32],
            x: [4; 32],
            y: [5; 32],
        })
    }

    /// The FIDO2-primary shape: the marker and a primary and recovery key.
    fn fido_header() -> Header {
        header(vec![
            (0, Token::RecoveryKey { keyslot: Some(0) }),
            (1, fido(FidoRole::Primary, 1, 7)),
            (2, fido(FidoRole::Recovery, 2, 8)),
        ])
    }

    fn tpm_pin(keyslot: u8) -> Token {
        use td_protector::pin::PinPcrs;
        use td_protector::protected::TpmPin;
        use td_tpm::SealedObject;
        Token::TpmPin(TpmPin {
            keyslot: Some(keyslot),
            pcrs: PinPcrs::Selector,
            primary: [0x0b; 34],
            salt: [9; 32],
            sealed: SealedObject {
                public: vec![1; 8],
                private: vec![2; 8],
            },
        })
    }

    /// The TPM-primary shape: the marker and a tpm-pin token.
    fn tpm_header() -> Header {
        header(vec![
            (0, Token::RecoveryKey { keyslot: Some(0) }),
            (1, tpm_pin(1)),
        ])
    }

    struct Fake {
        reads: Vec<Read>,
        standings: Vec<Option<[u8; 32]>>,
        kernel: Result<Vec<u8>, String>,
        template: Result<Vec<u8>, String>,
        disk: Disk,
        /// Applied to the disk at the second `esp()`, the re-read after `13`.
        poke: Option<fn(&mut [u8])>,
        esp_calls: usize,
        /// What the worker asked for, in order.
        log: Vec<&'static str>,
        /// Each keyslot test's slot and key, and the keyslot's answer.
        tested: Vec<(u8, Vec<u8>)>,
        opens: Result<Tested, String>,
        /// The header and deadline each test was given.
        expected: Vec<(Header, Instant)>,
        /// The upgrade view's volume beside each header read.
        volume: luks2::Volume,
        /// The headers the upgrade views read, in turn; `reads` when empty.
        views: Vec<Read>,
        tpm: Result<(), TpmRefusal>,
        /// cryptsetup's metadata; `None` dumps the latest read's header.
        metadata: Option<Vec<u8>>,
        /// Each commit, and whether the ESP held the new pair by then.
        commits: Vec<(Step, bool)>,
        /// The commit that fails, by its index.
        fails: Option<usize>,
    }

    impl Fake {
        fn new(read: Read) -> Self {
            Self {
                reads: vec![read],
                standings: vec![Some(ID)],
                kernel: Ok(vec![0x4b; 3000]),
                template: Ok(vec![0x54; 2000]),
                disk: esp_image(&[1; 5000], &[2; 4000], SECTORS),
                poke: None,
                esp_calls: 0,
                log: Vec::new(),
                tested: Vec::new(),
                opens: Ok(Tested::Opened),
                expected: Vec::new(),
                volume: luks2::Volume {
                    segment: Some(SEGMENT),
                    reencryption: false,
                    luks2: vec![0, 1],
                    reencrypt_modes: Vec::new(),
                },
                views: Vec::new(),
                tpm: Ok(()),
                metadata: None,
                commits: Vec::new(),
                fails: None,
            }
        }

        /// The `n`th of `values`, the last one repeating.
        fn nth<T: Clone>(values: &[T], n: usize) -> T {
            values.get(n).or(values.last()).cloned().unwrap()
        }
    }

    impl Host for Fake {
        type Disk = Disk;
        fn header(&mut self) -> Read {
            let n = self.log.iter().filter(|&&what| what == "header").count();
            self.log.push("header");
            Self::nth(&self.reads, n)
        }
        fn uuid(&self) -> &str {
            UUID
        }
        fn standing(&mut self) -> Option<[u8; 32]> {
            let n = self.log.iter().filter(|&&what| what == "standing").count();
            self.log.push("standing");
            Self::nth(&self.standings, n)
        }
        fn kernel(&mut self, id: &[u8; 32]) -> Result<Vec<u8>, String> {
            assert_eq!(id, &ID);
            self.log.push("kernel");
            self.kernel.clone()
        }
        fn template(&mut self) -> Result<Vec<u8>, String> {
            self.log.push("template");
            self.template.clone()
        }
        fn trusted_key(&mut self) -> Result<Vec<u8>, String> {
            self.log.push("key");
            Ok(KEY.to_vec())
        }
        fn esp(&mut self) -> Result<td_fat::Esp<Disk>, String> {
            self.log.push("esp");
            self.esp_calls += 1;
            if self.esp_calls == 2 {
                if let Some(poke) = self.poke {
                    poke(&mut self.disk.0.borrow_mut());
                }
            }
            td_fat::Esp::open(self.disk.clone(), BASE, SECTORS * 512).map_err(|e| e.to_string())
        }
        fn test_keyslot(
            &mut self,
            slot: u8,
            key: &[u8],
            expected: &Header,
            deadline: Instant,
        ) -> Result<Tested, String> {
            self.log.push("test");
            self.tested.push((slot, key.to_vec()));
            self.expected.push((expected.clone(), deadline));
            self.opens.clone()
        }
        fn upgrade_view(&mut self) -> Option<Result<UpgradeView, String>> {
            let n = self.log.iter().filter(|&&what| what == "view").count();
            self.log.push("view");
            let reads = if self.views.is_empty() {
                &self.reads
            } else {
                &self.views
            };
            let header = Self::nth(reads, n)?;
            Some(header.map(|header| UpgradeView {
                header,
                volume: self.volume.clone(),
                json_len: 3000,
            }))
        }
        fn tpm(&mut self) -> Result<(), TpmRefusal> {
            self.log.push("tpm");
            self.tpm
        }
        fn metadata(&mut self) -> Result<Vec<u8>, String> {
            self.log.push("metadata");
            if let Some(metadata) = &self.metadata {
                return Ok(metadata.clone());
            }
            match self.reads.last().cloned().flatten() {
                Some(Ok(header)) => Ok(dump(&header)),
                _ => Err("no header".into()),
            }
        }
        fn commit(&mut self, step: &Step, _: Instant) -> Result<(), String> {
            self.log.push("commit");
            let written = files(&self.disk).1 == selector(true);
            self.commits.push((step.clone(), written));
            if self.fails == Some(self.commits.len() - 1) {
                return Err("cryptsetup's header commit failed".into());
            }
            Ok(())
        }
    }

    /// The data segment's digest and salt as the dumps write them.
    const DIGEST: &str = "dms=";
    const SALT: &str = "c2FsdA==";
    const SEGMENT: [u8; 32] = [0; 32];

    fn segment() -> [u8; 32] {
        luks2::segment_digest(DIGEST, SALT)
    }

    /// `luksDump --dump-json-metadata` of `header`, its keyslots of type
    /// `luks2` and one digest naming the data segment.
    fn dump(header: &Header) -> Vec<u8> {
        let quoted = |slots: &[u8]| {
            slots
                .iter()
                .map(|slot| format!("\"{slot}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let keyslots: Vec<String> = header
            .keyslots
            .iter()
            .map(|slot| format!("\"{slot}\": {{\"type\": \"luks2\"}}"))
            .collect();
        let mut tokens: Vec<(u8, String)> = header
            .tokens
            .iter()
            .map(|(number, token)| (*number, token.encode()))
            .chain(
                header
                    .protected
                    .iter()
                    .map(|(number, token)| (*number, token.encode().unwrap())),
            )
            .collect();
        tokens.sort();
        let tokens: Vec<String> = tokens
            .iter()
            .map(|(number, json)| format!("\"{number}\": {json}"))
            .collect();
        format!(
            "{{\"keyslots\": {{{}}}, \"tokens\": {{{}}}, \"segments\": {{\"0\": {{\"type\": \"crypt\"}}}}, \"digests\": {{\"0\": {{\"type\": \"pbkdf2\", \"keyslots\": [{}], \"segments\": [\"0\"], \"digest\": \"{DIGEST}\", \"salt\": \"{SALT}\"}}}}, \"config\": {{\"json_size\": \"12288\"}}}}",
            keyslots.join(", "),
            tokens.join(", "),
            quoted(&header.keyslots)
        )
        .into_bytes()
    }

    /// A disk security key's script: which token it selects and the
    /// retries it reports, the failure before or after its one step, and
    /// what the worker handed it.
    struct Scripted {
        token: usize,
        retries: u8,
        before: Option<DiskError>,
        after: Option<DiskError>,
        /// The identify description and the authorize step's, as given.
        identify: Vec<u8>,
        authorize: Option<Vec<u8>>,
        deadline: Option<Instant>,
        /// Each connect's answer, the last repeating; each creation's
        /// exclusion list and the steps it presented.
        connects: Vec<Result<(), Failure>>,
        excluded: Vec<Vec<Vec<u8>>>,
        presented: Vec<Vec<u8>>,
        /// A creation's error after this many of its four steps.
        fails: Option<(usize, DiskError)>,
    }

    impl Scripted {
        fn selecting(token: usize) -> Self {
            Self {
                token,
                retries: 8,
                before: None,
                after: None,
                identify: Vec::new(),
                authorize: None,
                deadline: None,
                connects: vec![Ok(())],
                excluded: Vec::new(),
                presented: Vec::new(),
                fails: None,
            }
        }
    }

    /// A COSE ES256 key at P-256's base point, which td-fido admits.
    fn cose() -> Vec<u8> {
        let x = [
            0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4,
            0x40, 0xf2, 0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45,
            0xd8, 0x98, 0xc2, 0x96,
        ];
        let y = [
            0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f,
            0x9e, 0x16, 0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68,
            0x37, 0xbf, 0x51, 0xf5,
        ];
        let mut cose = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
        cose.extend(x);
        cose.extend([0x22, 0x58, 0x20]);
        cose.extend(y);
        cose
    }

    /// The hmac-secret output every scripted assertion gives.
    const OUTPUT: [u8; 32] = [0x6f; 32];

    impl Keys for Scripted {
        fn authorize(
            &mut self,
            deadline: Instant,
            tokens: &[fido_disk::Token<'_>],
            volume: &[u8; 16],
            identify: &[u8],
            steps: &mut Presenter<'_>,
        ) -> Result<Released, DiskError> {
            self.identify = identify.to_vec();
            self.deadline = Some(deadline);
            if let Some(error) = self.before.take() {
                return Err(error);
            }
            let (_pin, described) = steps
                .authorize(self.token, self.retries)
                .map_err(|_| DiskError::Ended)?;
            self.authorize = Some(described);
            if let Some(error) = self.after.take() {
                return Err(error);
            }
            let credential = tokens[self.token].credential;
            Ok(Released {
                token: self.token,
                passphrase: fido_disk::passphrase(&OUTPUT, volume, credential).unwrap(),
            })
        }

        fn connect(&mut self, _: Instant) -> Result<(), Failure> {
            let n = self.excluded.len();
            self.connects
                .get(n)
                .or(self.connects.last())
                .cloned()
                .unwrap()
        }

        /// Credential `0xc0 + n` for the `n`th new key: create, prove,
        /// repeat at one fewer retry, and probe.
        fn create(
            &mut self,
            deadline: Instant,
            excluded: &[&[u8]],
            volume: &[u8; 16],
            steps: &mut Ceremony<'_>,
        ) -> Result<Created, DiskError> {
            self.deadline = Some(deadline);
            self.excluded
                .push(excluded.iter().map(|id| id.to_vec()).collect());
            let credential = vec![0xc0 + self.excluded.len() as u8; 16];
            let failing = |done: usize, fails: &mut Option<(usize, DiskError)>| match fails.take() {
                Some((at, error)) if at == done => Err(error),
                other => {
                    *fails = other;
                    Ok(())
                }
            };
            failing(0, &mut self.fails)?;
            for (done, step) in [
                CreateStep::Create,
                CreateStep::Prove(&credential),
                CreateStep::Repeat(&credential),
            ]
            .into_iter()
            .enumerate()
            {
                let retries = if done == 2 { 7 } else { 8 };
                let (_pin, described) = steps.pin(step, retries).map_err(|_| DiskError::Ended)?;
                self.presented.push(described);
                failing(done + 1, &mut self.fails)?;
            }
            let described = steps.probe(&credential).map_err(|_| DiskError::Ended)?;
            self.presented.push(described);
            failing(4, &mut self.fails)?;
            Ok(Created {
                passphrase: fido_disk::passphrase(&OUTPUT, volume, &credential).unwrap(),
                credential,
                cose: cose(),
                salt: [0x5c; 32],
            })
        }
    }

    /// The template, a marker, the key and the identity: what the test's
    /// `prepare` returns, so the staged initrd can be checked.
    fn prepare(template: &[u8], key: &[u8], uuid: &str, measured: bool) -> Result<Vec<u8>, String> {
        let mut out = template.to_vec();
        out.extend_from_slice(b"|");
        out.extend_from_slice(key);
        out.extend_from_slice(uuid.as_bytes());
        out.push(u8::from(measured));
        Ok(out)
    }

    fn update(form: SelectorForm, id: &[u8; 32]) -> Request {
        Request::new(
            NONCE,
            1000,
            Operation::SelectorUpdate {
                key: ApprovalKey::new(*b"36").unwrap(),
                form,
                current: hex(id),
            },
        )
        .unwrap()
    }

    /// Root's side: everything the worker sent, framed, after root sends
    /// `description` and acknowledges each invitation, then `secrets` in
    /// turn, each after the presentation of a step that asks for one.
    fn root(
        stream: UnixStream,
        description: Option<Vec<u8>>,
        secrets: Vec<Vec<u8>>,
    ) -> Vec<Vec<u8>> {
        let mut secrets = secrets.into_iter();
        let mut stream = stream;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut seen = Vec::new();
        let read = |stream: &mut UnixStream| -> Option<Vec<u8>> {
            let mut header = [0; 2];
            stream.read_exact(&mut header).ok()?;
            let mut frame = vec![0; usize::from(u16::from_be_bytes(header))];
            stream.read_exact(&mut frame).ok()?;
            Some(frame)
        };
        let send = |stream: &mut UnixStream, bytes: &[u8]| {
            let _ = stream.write_all(&(bytes.len() as u16).to_be_bytes());
            let _ = stream.write_all(bytes);
        };
        let Some(baseline) = read(&mut stream) else {
            return seen;
        };
        seen.push(baseline);
        if let Some(description) = description {
            send(&mut stream, &description);
        }
        while let Some(frame) = read(&mut stream) {
            if let Some(tag @ (PRESENT | COMMIT)) = frame.first().copied() {
                let mut answer = frame.clone();
                answer[0] = tag + 1;
                send(&mut stream, &answer);
                let secret_step = Request::decode(&frame[33..])
                    .ok()
                    .and_then(|request| {
                        request
                            .login_step()
                            .or(request.upgrade_step().and_then(UpgradeStep::login))
                    })
                    .is_some_and(LoginStep::asks_pin);
                if tag == PRESENT && secret_step {
                    if let Some(secret) = secrets.next() {
                        let mut message = vec![frames::SECRET];
                        message.extend(secret);
                        send(&mut stream, &message);
                    }
                }
            }
            seen.push(frame);
        }
        seen
    }

    struct Run {
        result: Result<(), Failure>,
        frames: Vec<Vec<u8>>,
        log: Vec<&'static str>,
        disk: Disk,
        tested: Vec<(u8, Vec<u8>)>,
        expected: Vec<(Header, Instant)>,
        commits: Vec<(Step, bool)>,
    }

    fn run_with(host: Fake, description: Option<Vec<u8>>, margin: Duration) -> Run {
        let mut keys = Scripted::selecting(0);
        proved(host, &mut keys, description, Vec::new(), margin)
    }

    fn proved(
        mut host: Fake,
        keys: &mut Scripted,
        description: Option<Vec<u8>>,
        secrets: Vec<Vec<u8>>,
        margin: Duration,
    ) -> Run {
        let (worker, parent) = UnixStream::pair().unwrap();
        let root = std::thread::spawn(move || root(parent, description, secrets));
        let started = Instant::now();
        let mut wire = Wire::new(worker, started + LOGIN_LONGEST).unwrap();
        let mut rounds = 0u8;
        let result = operate(
            &mut wire,
            &mut host,
            keys,
            1000,
            started,
            margin,
            &prepare,
            &mut || {
                rounds += 1;
                Ok([rounds; 32])
            },
        );
        drop(wire);
        Run {
            result,
            frames: root.join().unwrap(),
            log: host.log,
            disk: host.disk,
            tested: host.tested,
            expected: host.expected,
            commits: host.commits,
        }
    }

    fn run(host: Fake, form: SelectorForm) -> Run {
        run_with(
            host,
            Some(update(form, &ID).encode()),
            Duration::from_secs(5),
        )
    }

    /// The pair, and the staged pair, the disk now names.
    fn files(disk: &Disk) -> (Vec<u8>, Vec<u8>, Option<(Vec<u8>, Vec<u8>)>) {
        let esp = td_fat::Esp::open(disk.clone(), BASE, SECTORS * 512).unwrap();
        let read = |file: &td_fat::FileEntry| {
            let mut out = Vec::new();
            esp.read(file, &mut |chunk| {
                out.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();
            out
        };
        let boot = esp.boot().clone();
        (
            read(&boot.pair.kernel),
            read(&boot.pair.initrd),
            boot.staged
                .map(|staged| (read(&staged.kernel), read(&staged.initrd))),
        )
    }

    fn selector(measured: bool) -> Vec<u8> {
        prepare(&[0x54; 2000], KEY, UUID, measured).unwrap()
    }

    /// A converged device-bound header: the device-bound token naming
    /// keyslot 1, and `pending` fido2 tokens a crashed phase 1 left.
    fn converged(pending: &[u8]) -> Header {
        use td_protector::token::{self as device, Role};
        let mut header = header(Vec::new());
        header.tokens = vec![(
            0,
            device::Token::new(
                1,
                Role::DeviceBound,
                td_tpm::SealedObject {
                    public: vec![0, 8, 1],
                    private: vec![1, 1],
                },
            )
            .unwrap(),
        )];
        for (at, byte) in pending.iter().enumerate() {
            header.protected.push((
                at as u8 + 1,
                Token::Fido2(Fido2 {
                    keyslot: None,
                    role: FidoRole::Recovery,
                    credential: vec![*byte; 16],
                    salt: [1; 32],
                    x: [2; 32],
                    y: [3; 32],
                }),
            ));
        }
        header
    }

    /// The same header in U0: `pending` new keys' tokens and `td-upgrade`.
    fn upgrading(pending: &[u8]) -> Header {
        let mut header = converged(pending);
        header.protected.push((
            9,
            Token::Upgrade {
                shape: Shape::Tpm,
                from: segment(),
            },
        ));
        header
    }

    fn device_bound_host(header: Header) -> Fake {
        let mut host = Fake::new(Some(Ok(header)));
        host.volume.segment = Some(segment());
        host
    }

    fn upgrade_request(shape: UpgradeShape, count: u8) -> Request {
        Request::new(
            NONCE,
            1000,
            Operation::Upgrade {
                account: 1000,
                shape,
                count,
                current: hex(&ID),
                step: UpgradeStep::Review,
                approval: Some(ApprovalKey::new(*b"47").unwrap()),
            },
        )
        .unwrap()
    }

    fn cancel_request() -> Request {
        Request::new(
            NONCE,
            1000,
            Operation::UpgradeCancel {
                key: ApprovalKey::new(*b"47").unwrap(),
            },
        )
        .unwrap()
    }

    /// Runs `description` against `host` with `keys`, a PIN for each PIN
    /// step.
    fn upgraded(host: Fake, keys: &mut Scripted, description: &Request) -> Run {
        proved(
            host,
            keys,
            Some(description.encode()),
            vec![b"1234".to_vec(); 6],
            Duration::from_secs(5),
        )
    }

    /// The steps each `PRESENT` frame after the first presented.
    fn upgrade_steps(frames: &[Vec<u8>]) -> Vec<UpgradeStep> {
        frames
            .iter()
            .filter(|frame| frame.first() == Some(&PRESENT))
            .map(|frame| {
                Request::decode(&frame[33..])
                    .unwrap()
                    .upgrade_step()
                    .unwrap()
            })
            .collect()
    }

    fn pending_token(byte: u8, role: FidoRole) -> Token {
        let key = Es256PublicKey::from_cose(&cose()).unwrap();
        let (x, y) = key.coordinates();
        Token::Fido2(Fido2 {
            keyslot: None,
            role,
            credential: vec![byte; 16],
            salt: [0x5c; 32],
            x: *x,
            y: *y,
        })
    }

    fn import(token: Token) -> Step {
        Step::ImportToken {
            token,
            id: None,
            replace: false,
        }
    }

    #[test]
    fn an_upgrade_presents_its_review_and_each_ceremony_then_writes_phase_one() {
        for (shape, count, role, orphans) in [
            (UpgradeShape::Tpm, 2, FidoRole::Recovery, vec![0x71, 0x72]),
            (UpgradeShape::Fido2, 1, FidoRole::Primary, Vec::new()),
            (UpgradeShape::Tpm, 0, FidoRole::Recovery, vec![0x71]),
        ] {
            let host = device_bound_host(converged(&orphans));
            let mut keys = Scripted::selecting(0);
            let review = upgrade_request(shape, count);
            let run = upgraded(host, &mut keys, &review);
            let case = format!("{shape:?} {count}");
            assert_eq!(run.result, Ok(()), "{case}");
            // The review, then each key's connect, create, prove, repeat
            // and probe, its credential named from prove on.
            let presented = upgrade_steps(&run.frames);
            let mut expected = vec![UpgradeStep::Review];
            for key in 1..=count {
                let made = fingerprint(&[0xc0 + key; 16]);
                expected.extend(
                    [
                        LoginStep::Connect,
                        LoginStep::Create { retries: 8 },
                        LoginStep::Prove {
                            key: made,
                            retries: 8,
                        },
                        LoginStep::Repeat {
                            key: made,
                            retries: 7,
                        },
                        LoginStep::Probe { key: made },
                    ]
                    .map(|step| UpgradeStep::Key { key, step }),
                );
            }
            assert_eq!(presented, expected, "{case}");
            assert_eq!(&run.frames[1][33..], &review.encode()[..]);
            // The commit round names the last step, then success.
            let commit = &run.frames[run.frames.len() - 2];
            assert_eq!(commit[0], COMMIT);
            assert_eq!(
                Request::decode(&commit[33..]).unwrap().upgrade_step(),
                expected.last().copied()
            );
            assert_eq!(run.frames.last().unwrap(), &[SUCCESS]);
            // td-fido bound each PIN and probe step to what was presented.
            let bodies: Vec<&[u8]> = run
                .frames
                .iter()
                .filter(|frame| frame.first() == Some(&PRESENT))
                .map(|frame| &frame[33..])
                .collect();
            for described in &keys.presented {
                assert!(bodies.contains(&described.as_slice()), "{case}");
            }
            // Each key excludes the earlier ones' credentials.
            let excluded: Vec<Vec<Vec<u8>>> = (0..count)
                .map(|n| (1..=n).map(|k| vec![0xc0 + k; 16]).collect())
                .collect();
            assert_eq!(keys.excluded, excluded, "{case}");
            // Every read before the review, the TPM's in the TPM shape alone.
            let tpm = usize::from(shape == UpgradeShape::Tpm);
            let mut reads = vec!["header", "standing", "view"];
            reads.extend(["tpm"].iter().take(tpm));
            reads.extend(["kernel", "template", "key", "esp"]);
            assert_eq!(run.log[..reads.len()], reads[..], "{case}");
            let after: Vec<&str> = run.log[reads.len()..]
                .iter()
                .copied()
                .filter(|what| *what != "commit")
                .collect();
            assert_eq!(after, ["header", "standing", "esp", "view", "metadata"]);
            // The orphans' removals before the selector, then each key's
            // pending token and last `td-upgrade`.
            let mut commits: Vec<(Step, bool)> = (1..=orphans.len() as u8)
                .map(|token| (Step::RemoveToken { token }, false))
                .collect();
            for key in 1..=count {
                commits.push((import(pending_token(0xc0 + key, role)), true));
            }
            let shape = match shape {
                UpgradeShape::Tpm => Shape::Tpm,
                UpgradeShape::Fido2 => Shape::Fido2,
            };
            commits.push((
                import(Token::Upgrade {
                    shape,
                    from: segment(),
                }),
                true,
            ));
            assert_eq!(run.commits, commits, "{case}");
            // The selector in its direct form, with the measurement policy.
            let (kernel, initrd, staged) = files(&run.disk);
            assert_eq!((kernel, initrd), (vec![0x4b; 3000], selector(true)));
            assert_eq!(staged, None);
        }
    }

    #[test]
    fn an_upgrade_refuses_before_its_review() {
        let review = upgrade_request(UpgradeShape::Tpm, 1);
        let refused = |host: Fake, description: &Request| {
            let run = upgraded(host, &mut Scripted::selecting(0), description);
            assert!(upgrade_steps(&run.frames).is_empty());
            assert!(run.commits.is_empty());
            run
        };
        // Not converged: STATE, before the TPM or any selector read.
        let mut bare = converged(&[]);
        bare.tokens.clear();
        let mut extra = converged(&[]);
        extra.keyslots.push(2);
        for header in [bare, extra] {
            let mut host = device_bound_host(header);
            host.volume.luks2 = host.reads[0].clone().unwrap().unwrap().keyslots;
            let run = refused(host, &review);
            assert_eq!(run.result, Err(Failure::State));
            assert_eq!(run.frames.last().unwrap(), &[FAILURE, 0x21]);
        }
        // The TPM shape's TPM, each detail.
        for (refusal, detail) in [
            (TpmRefusal::Absent, 1),
            (TpmRefusal::NoSha256Bank, 2),
            (TpmRefusal::LockoutSet, 3),
        ] {
            let mut host = device_bound_host(converged(&[]));
            host.tpm = Err(refusal);
            let run = refused(host, &review);
            assert_eq!(run.result, Err(Failure::Tpm(refusal)));
            assert_eq!(run.frames.last().unwrap(), &[FAILURE, 0x27, detail]);
            assert_eq!(run.log.last(), Some(&"tpm"));
        }
        // The FIDO2 shape reads no TPM.
        let mut host = device_bound_host(converged(&[]));
        host.tpm = Err(TpmRefusal::Absent);
        let fido = upgrade_request(UpgradeShape::Fido2, 1);
        let run = upgraded(host, &mut Scripted::selecting(0), &fido);
        assert_eq!(run.result, Ok(()));
        assert!(!run.log.contains(&"tpm"));
        // A header gone or refused at the view: VOLUME; changed: CHANGED.
        for (view, failure) in [
            (None, Failure::Volume),
            (Some(Err("refused".into())), Failure::Volume),
            (Some(Ok(converged(&[5]))), Failure::Changed),
        ] {
            let mut host = device_bound_host(converged(&[]));
            host.views = vec![view];
            let run = refused(host, &review);
            assert_eq!(run.frames.last().unwrap(), &failure.frame());
        }
        // The selector's reads, as an update's.
        let mut kernel = device_bound_host(converged(&[]));
        kernel.kernel = Err("bzImage does not match the manifest".into());
        assert_eq!(refused(kernel, &review).result, Err(Failure::Deployment));
        // A description the baseline does not admit: INTERNAL, reading
        // nothing.
        let key_first = Request::new(
            NONCE,
            1000,
            Operation::Upgrade {
                account: 1000,
                shape: UpgradeShape::Tpm,
                count: 1,
                current: hex(&ID),
                step: UpgradeStep::Key {
                    key: 1,
                    step: LoginStep::Connect,
                },
                approval: None,
            },
        )
        .unwrap();
        let other = Request::new(
            NONCE,
            1000,
            Operation::Upgrade {
                account: 1000,
                shape: UpgradeShape::Tpm,
                count: 1,
                current: hex(&[0xcd; 32]),
                step: UpgradeStep::Review,
                approval: Some(ApprovalKey::new(*b"47").unwrap()),
            },
        )
        .unwrap();
        for (header, description) in [
            (converged(&[]), key_first),
            (converged(&[]), other),
            (tpm_header(), review.clone()),
            (upgrading(&[]), review.clone()),
        ] {
            let host = device_bound_host(header);
            let run = refused(host, &description);
            assert_eq!(run.result, Err(Failure::Internal));
            assert_eq!(run.log, ["header", "standing"]);
        }
    }

    #[test]
    fn a_ceremony_failure_ends_the_upgrade_with_its_kind_and_writes_nothing() {
        let review = upgrade_request(UpgradeShape::Tpm, 2);
        for (fails, connects, failure, frame) in [
            (
                Some((
                    0,
                    DiskError::Failed(FidoError::Status(Status::CredentialExcluded)),
                )),
                vec![Ok(())],
                Failure::Excluded,
                vec![FAILURE, 0x11],
            ),
            (
                Some((4, DiskError::Unselectable)),
                vec![Ok(())],
                Failure::Unprobed,
                vec![FAILURE, 0x06, 0x06],
            ),
            (
                Some((2, DiskError::Failed(FidoError::Status(Status::PinInvalid)))),
                vec![Ok(())],
                Failure::WrongPin(8),
                vec![FAILURE, 0x01, 8],
            ),
            (
                Some((3, DiskError::Mismatch)),
                vec![Ok(())],
                Failure::Failed,
                vec![FAILURE, 0x0f],
            ),
            (
                None,
                vec![Ok(()), Err(Failure::OneKey)],
                Failure::OneKey,
                vec![FAILURE, 0x05],
            ),
        ] {
            let host = device_bound_host(converged(&[]));
            let mut keys = Scripted::selecting(0);
            keys.fails = fails;
            keys.connects = connects;
            let run = upgraded(host, &mut keys, &review);
            assert_eq!(run.result, Err(failure));
            assert_eq!(run.frames.last().unwrap(), &frame);
            assert!(!run.frames.iter().any(|f| f.first() == Some(&COMMIT)));
            assert!(run.commits.is_empty());
            assert_eq!(files(&run.disk).0, vec![1; 5000]);
        }
    }

    #[test]
    fn after_its_commit_round_an_upgrade_writes_only_over_what_it_presented() {
        let review = upgrade_request(UpgradeShape::Fido2, 1);
        // The header, the standing or the ESP changed: CHANGED, nothing
        // written, and the worker exits successfully.
        // The header's re-read after `13` differs; the view before the
        // review still reads the first.
        let mut header = device_bound_host(converged(&[]));
        header.reads = vec![Some(Ok(converged(&[]))), Some(Ok(converged(&[4])))];
        header.views = vec![Some(Ok(converged(&[])))];
        // The view's re-read after `13` differs, the header's does not.
        let mut view = device_bound_host(converged(&[]));
        view.views = vec![Some(Ok(converged(&[]))), Some(Ok(converged(&[4])))];
        let mut standing = device_bound_host(converged(&[]));
        standing.standings = vec![Some(ID), None];
        let mut esp = device_bound_host(converged(&[]));
        // As the selector update's sector test: BOOTX64.EFI's
        // creation-time tenths in EFI/BOOT's first sector.
        esp.poke = Some(|disk| {
            let (reserved, fat, _) = layout(disk);
            disk[BASE as usize + (reserved + 2 * fat + 2) * 512 + 2 * 32 + 13] = 1;
        });
        let mut disagrees = device_bound_host(converged(&[]));
        disagrees.metadata = Some(dump(&converged(&[6])));
        // Only the metadata's disagreement reaches cryptsetup's read.
        for (host, metadata) in [
            (header, false),
            (view, false),
            (standing, false),
            (esp, false),
            (disagrees, true),
        ] {
            let run = upgraded(host, &mut Scripted::selecting(0), &review);
            assert_eq!(run.result, Ok(()));
            assert_eq!(run.frames.last().unwrap(), &[FAILURE, 0x28]);
            assert_eq!(run.log.contains(&"metadata"), metadata, "{:?}", run.log);
            assert!(run.commits.is_empty());
            assert_eq!(files(&run.disk).0, vec![1; 5000]);
        }
        // A commit that fails is FAILED: uncertain to root.
        let mut host = device_bound_host(converged(&[0x71]));
        host.fails = Some(1);
        let run = upgraded(host, &mut Scripted::selecting(0), &review);
        assert_eq!(run.result, Err(Failure::Failed));
        assert_eq!(run.frames.last().unwrap(), &[FAILURE, 0x0f]);
        assert_eq!(run.commits.len(), 2);
    }

    #[test]
    fn a_cancellation_removes_td_upgrade_then_the_pending_tokens() {
        let cancel = cancel_request();
        let host = device_bound_host(upgrading(&[0xc1, 0xc2]));
        let run = upgraded(host, &mut Scripted::selecting(0), &cancel);
        assert_eq!(run.result, Ok(()));
        assert_eq!(run.frames[0][1], 2);
        assert_eq!(run.frames[1][0], PRESENT);
        assert_eq!(&run.frames[1][33..], &cancel.encode()[..]);
        assert_eq!(run.frames[2][0], COMMIT);
        assert_eq!(&run.frames[2][33..], &cancel.encode()[..]);
        assert_eq!(run.frames[3], [SUCCESS]);
        let removed: Vec<Step> = run.commits.into_iter().map(|(step, _)| step).collect();
        assert_eq!(removed, [9, 1, 2].map(|token| Step::RemoveToken { token }));
        assert_eq!(
            run.log,
            ["header", "standing", "view", "header", "metadata", "commit", "commit", "commit"]
        );
        // Past U0: STATE before the presentation.
        let mut past = device_bound_host(upgrading(&[0xc1]));
        past.volume.segment = Some([9; 32]);
        let run = upgraded(past, &mut Scripted::selecting(0), &cancel);
        assert_eq!(run.result, Err(Failure::State));
        assert_eq!(run.frames.len(), 2);
        // Not upgrading: INTERNAL, reading nothing more.
        let run = upgraded(
            device_bound_host(converged(&[])),
            &mut Scripted::selecting(0),
            &cancel,
        );
        assert_eq!(run.result, Err(Failure::Internal));
        assert_eq!(run.log, ["header", "standing"]);
        // Changed after the commit round: CHANGED, nothing removed.
        let mut changed = device_bound_host(upgrading(&[0xc1]));
        changed.reads = vec![Some(Ok(upgrading(&[0xc1]))), Some(Ok(upgrading(&[0xc3])))];
        let run = upgraded(changed, &mut Scripted::selecting(0), &cancel);
        assert_eq!(run.result, Ok(()));
        assert_eq!(run.frames.last().unwrap(), &[FAILURE, 0x28]);
        assert_eq!(run.log, ["header", "standing", "view", "header"]);
        assert!(run.commits.is_empty());
        // Cryptsetup's metadata that disagrees: CHANGED too.
        let mut disagrees = device_bound_host(upgrading(&[0xc1]));
        disagrees.metadata = Some(dump(&upgrading(&[0xc3])));
        let run = upgraded(disagrees, &mut Scripted::selecting(0), &cancel);
        assert_eq!(run.frames.last().unwrap(), &[FAILURE, 0x28]);
        assert_eq!(run.log.last(), Some(&"metadata"));
        assert!(run.commits.is_empty());
    }

    #[test]
    fn a_tpm_primary_volume_stages_the_running_pair_and_says_so_last() {
        let host = Fake::new(Some(Ok(tpm_header())));
        let run = run(host, SelectorForm::Staged);
        assert_eq!(run.result, Ok(()));
        let description = update(SelectorForm::Staged, &ID).encode();
        let mut baseline = vec![0x18, 3, 1, 0, 0];
        baseline.extend_from_slice(&ID);
        assert_eq!(run.frames[0], baseline);
        // One presentation, one commit round, both of exactly the description,
        // then success.
        assert_eq!(run.frames.len(), 4);
        assert_eq!(run.frames[1][0], PRESENT);
        assert_eq!(&run.frames[1][1..33], &[1; 32]);
        assert_eq!(&run.frames[1][33..], &description[..]);
        assert_eq!(run.frames[2][0], COMMIT);
        assert_eq!(&run.frames[2][1..33], &[2; 32]);
        assert_eq!(&run.frames[2][33..], &description[..]);
        assert_eq!(run.frames[3], [SUCCESS]);
        // Every read before the presentation; the re-reads after the commit.
        assert_eq!(
            run.log,
            [
                "header", "standing", "kernel", "template", "key", "esp", "header", "standing",
                "esp"
            ]
        );
        let (kernel, initrd, staged) = files(&run.disk);
        assert_eq!((kernel, initrd), (vec![1; 5000], vec![2; 4000]));
        assert_eq!(staged, Some((vec![0x4b; 3000], selector(true))));
    }

    #[test]
    fn the_direct_forms_replace_the_pair() {
        for (header, form, measured, volume) in [
            (header(Vec::new()), SelectorForm::DeviceBound, false, 1),
            (fido_header(), SelectorForm::Fido2, true, 4),
        ] {
            let run = run(Fake::new(Some(Ok(header))), form);
            assert_eq!(run.result, Ok(()), "{form:?}");
            assert_eq!(run.frames[0][1], volume);
            assert_eq!(run.frames.last().unwrap(), &[SUCCESS]);
            let (kernel, initrd, staged) = files(&run.disk);
            assert_eq!(kernel, vec![0x4b; 3000]);
            assert_eq!(initrd, selector(measured));
            assert_eq!(staged, None);
        }
    }

    #[test]
    fn the_baseline_names_the_volume_its_keys_and_the_running_standing() {
        let fingerprint = |byte| super::fingerprint(&[byte; 16]);
        let fido = Baseline::new(&Some(Ok(fido_header())), Some(ID));
        let mut frame = vec![0x18, 4, 1, 2, 1];
        frame.extend_from_slice(&fingerprint(7));
        frame.push(2);
        frame.extend_from_slice(&fingerprint(8));
        frame.push(0);
        frame.extend_from_slice(&ID);
        assert_eq!(fido.frame(), frame);
        let mut upgrading = header(vec![(0, Token::RecoveryKey { keyslot: Some(0) })]);
        upgrading.protected.push((
            3,
            Token::Upgrade {
                shape: Shape::Tpm,
                from: [1; 32],
            },
        ));
        for (read, current, expected) in [
            (None, Some(ID), vec![0x18, 0, 0, 0, 0]),
            (Some(Err("refused".into())), None, vec![0x18, 5, 0, 0, 1]),
            (Some(Ok(header(Vec::new()))), None, vec![0x18, 1, 0, 0, 1]),
            // A protected header outside both shapes is one td's planner refuses.
            (
                Some(Ok(header(vec![(1, tpm_pin(1))]))),
                None,
                vec![0x18, 5, 0, 0, 1],
            ),
        ] {
            let frame = Baseline::new(&read, current).frame();
            assert_eq!(frame[..expected.len()], expected[..], "{read:?}");
        }
        assert_eq!(
            Baseline::new(&Some(Ok(upgrading)), None).volume,
            Volume::Upgrading
        );
    }

    /// Root's first frame must be the description the baseline admits: any
    /// other ends the worker as INTERNAL before it reads anything else.
    #[test]
    fn a_description_the_baseline_does_not_admit_reads_nothing() {
        let other = [0xcd; 32];
        let owner = Request::new(
            NONCE,
            1001,
            Operation::SelectorUpdate {
                key: ApprovalKey::new(*b"36").unwrap(),
                form: SelectorForm::Staged,
                current: hex(&ID),
            },
        )
        .unwrap();
        for (read, standing, description) in [
            (
                tpm_header(),
                Some(ID),
                update(SelectorForm::DeviceBound, &ID).encode(),
            ),
            (
                tpm_header(),
                Some(ID),
                update(SelectorForm::Staged, &other).encode(),
            ),
            (
                tpm_header(),
                None,
                update(SelectorForm::Staged, &ID).encode(),
            ),
            (tpm_header(), Some(ID), owner.encode()),
            (
                header(Vec::new()),
                Some(ID),
                update(SelectorForm::Staged, &ID).encode(),
            ),
            (tpm_header(), Some(ID), b"TDCONS01".to_vec()),
        ] {
            let mut host = Fake::new(Some(Ok(read)));
            host.standings = vec![standing];
            let run = run_with(host, Some(description), Duration::from_secs(5));
            assert_eq!(run.result, Err(Failure::Internal));
            assert_eq!(run.log, ["header", "standing"]);
            assert_eq!(run.frames.last().unwrap(), &[FAILURE, 0x10]);
        }
    }

    #[test]
    fn each_read_before_the_presentation_has_its_kind() {
        let staged = || Fake::new(Some(Ok(tpm_header())));
        let mut kernel = staged();
        kernel.kernel = Err("bzImage does not match the manifest".into());
        let mut template = staged();
        template.template = Err("unreadable".into());
        let mut foreign = staged();
        foreign.disk = Disk(Rc::new(RefCell::new(vec![0; 4 << 20])));
        // More than the volume has free.
        let mut small = staged();
        small.kernel = Ok(vec![0x4b; 34 << 20]);
        for (host, failure) in [
            (kernel, Failure::Deployment),
            (template, Failure::Deployment),
            (foreign, Failure::Esp),
            (small, Failure::Esp),
        ] {
            let run = run(host, SelectorForm::Staged);
            assert_eq!(run.result, Err(failure));
            // No presentation, no commit: the baseline and the failure.
            assert_eq!(run.frames.len(), 2, "{failure:?}");
            assert_eq!(run.frames[1], [FAILURE, failure.kind()]);
        }
    }

    /// The commit invitation needs the writes' time left of the deadline.
    #[test]
    fn the_commit_margin_is_checked_before_the_invitation() {
        let host = Fake::new(Some(Ok(tpm_header())));
        let before = files(&host.disk);
        let run = run_with(
            host,
            Some(update(SelectorForm::Staged, &ID).encode()),
            SELECTOR_UPDATE_CEILING,
        );
        assert_eq!(run.result, Err(Failure::Timeout));
        assert_eq!(run.frames.len(), 3);
        assert_eq!(run.frames[1][0], PRESENT);
        assert_eq!(run.frames[2], [FAILURE, 0x08]);
        assert_eq!(files(&run.disk), before);
    }

    /// After root's `13` the header, the standing and the sector are read
    /// again; any change is `28` with nothing written, and the worker exits
    /// successfully after it.
    #[test]
    fn a_change_after_the_commit_is_definite_and_writes_nothing() {
        let changed_header = {
            let mut changed = tpm_header();
            changed.seqid = 2;
            changed
        };
        let mut header = Fake::new(Some(Ok(tpm_header())));
        header.reads.push(Some(Ok(changed_header)));
        let mut unreadable = Fake::new(Some(Ok(tpm_header())));
        unreadable.reads.push(Some(Err("refused".into())));
        let mut standing = Fake::new(Some(Ok(tpm_header())));
        standing.standings.push(None);
        let mut sector = Fake::new(Some(Ok(tpm_header())));
        // BOOTX64.EFI's creation-time tenths in EFI/BOOT's first sector, which
        // td-fat admits either way: the root's cluster is 2, EFI's 3, BOOT's
        // 4, one sector each after the reserved sectors and the two FATs.
        sector.poke = Some(|disk| {
            let (reserved, fat, _) = layout(disk);
            disk[BASE as usize + (reserved + 2 * fat + 2) * 512 + 2 * 32 + 13] = 1;
        });
        // Every free cluster marked bad in both FATs, EFI/BOOT untouched:
        // td-fat's space check, which writes nothing, now refuses.
        let mut space = Fake::new(Some(Ok(tpm_header())));
        space.poke = Some(|disk| {
            let (reserved, fat, clusters) = layout(disk);
            for copy in 0..2 {
                let base = BASE as usize + (reserved + copy * fat) * 512;
                for cluster in 2..clusters + 2 {
                    let at = base + cluster * 4;
                    if disk[at..at + 4] == [0; 4] {
                        disk[at..at + 4].copy_from_slice(&0x0fff_fff7u32.to_le_bytes());
                    }
                }
            }
        });
        for host in [header, unreadable, standing, sector, space] {
            let before = files(&host.disk);
            let run = run(host, SelectorForm::Staged);
            assert_eq!(run.result, Ok(()));
            assert_eq!(run.frames.len(), 4);
            assert_eq!(run.frames[3], [FAILURE, 0x28]);
            assert_eq!(files(&run.disk), before);
        }
    }

    /// The test ESP's reserved and FAT sectors and its cluster count, one
    /// sector a cluster.
    fn layout(disk: &[u8]) -> (usize, usize, usize) {
        let bpb = &disk[BASE as usize..][..512];
        let reserved = usize::from(u16::from_le_bytes([bpb[14], bpb[15]]));
        let fat = u32::from_le_bytes(bpb[36..40].try_into().unwrap()) as usize;
        let total = u32::from_le_bytes(bpb[32..36].try_into().unwrap()) as usize;
        (reserved, fat, total - reserved - 2 * fat)
    }

    /// A machine rooted at a scratch directory holding the volume's
    /// `current` link to `ID` and the running deployment's record.
    struct Rooted {
        machine: Machine,
        root: PathBuf,
    }

    impl Drop for Rooted {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn rooted(cmdline: &str) -> Rooted {
        let root = crate::scratch::path("storage-machine");
        let volume = root.join(VOLUME_MOUNT);
        std::fs::create_dir_all(volume.join(protocol::ATTEMPTS_DIR)).unwrap();
        std::fs::create_dir_all(volume.join(protocol::DEPLOYMENTS_DIR).join(hex(&ID))).unwrap();
        std::os::unix::fs::symlink(
            format!("{}{}", protocol::SELECTOR_PREFIX, hex(&ID)),
            volume.join(protocol::BOOT_DIR).join(protocol::CURRENT_SLOT),
        )
        .unwrap();
        std::fs::write(root.join(DEPLOYMENT), format!("{}\n", hex(&ID))).unwrap();
        Rooted {
            machine: Machine {
                uuid: UUID.into(),
                cmdline: cmdline.into(),
                partition: None,
                device: None,
                authority: None,
                root: root.clone(),
            },
            root,
        }
    }

    /// The running deployment stands when it is `current`, with no pending
    /// attempt and with bookkeeping; a record in another form, another
    /// `current`, a pending attempt or a bookkeeping failure leaves none.
    #[test]
    fn the_machine_stands_on_current_alone() {
        let mut host = rooted("quiet td.volume=x");
        assert_eq!(host.machine.standing(), Some(ID));
        assert!(host.machine.header().is_none());

        let mut host = rooted(&format!(
            "quiet {}",
            protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN
        ));
        assert_eq!(host.machine.standing(), None);

        let host = rooted("");
        let attempt = host
            .root
            .join(VOLUME_MOUNT)
            .join(protocol::ATTEMPTS_DIR)
            .join(hex(&ID));
        std::fs::write(&attempt, b"1\n").unwrap();
        let mut host = host;
        assert_eq!(host.machine.standing(), None);

        let mut host = rooted("");
        let current = host
            .root
            .join(VOLUME_MOUNT)
            .join(protocol::BOOT_DIR)
            .join(protocol::CURRENT_SLOT);
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(
            format!("{}{}", protocol::SELECTOR_PREFIX, "cd".repeat(32)),
            &current,
        )
        .unwrap();
        assert_eq!(host.machine.standing(), None);

        for record in [
            hex(&ID),
            format!("{}\n", hex(&ID).to_uppercase()),
            format!("{}\n", &hex(&ID)[2..]),
            format!("{}\n\n", hex(&ID)),
        ] {
            let mut host = rooted("");
            std::fs::write(host.root.join(DEPLOYMENT), record).unwrap();
            assert_eq!(host.machine.standing(), None);
        }
        let mut host = rooted("");
        std::fs::remove_file(host.root.join(DEPLOYMENT)).unwrap();
        assert_eq!(host.machine.standing(), None);
    }

    /// The template is the running root's; the trusted key the volume's.
    #[test]
    fn the_machine_reads_the_template_and_key_under_its_root() {
        let mut host = rooted("");
        assert!(host.machine.template().is_err());
        assert!(host.machine.trusted_key().is_err());
        let template = host.root.join(protocol::SELECTOR_TEMPLATE_PATH);
        std::fs::create_dir_all(template.parent().unwrap()).unwrap();
        std::fs::write(&template, b"template").unwrap();
        std::fs::write(
            host.root
                .join(VOLUME_MOUNT)
                .join(protocol::VOLUME_TRUSTED_KEY),
            KEY,
        )
        .unwrap();
        assert_eq!(host.machine.template().unwrap(), b"template");
        assert_eq!(host.machine.trusted_key().unwrap(), KEY);
    }

    /// After `td-boot authenticate`, the kernel is read only when its line
    /// is the ID's, the manifest hashes to the ID and the kernel to the
    /// manifest's one `bzImage` line.
    #[test]
    fn the_authenticated_kernel_is_current_manifests() {
        let host = rooted("");
        let kernel = b"kernel bytes".to_vec();
        let directory = host
            .root
            .join(VOLUME_MOUNT)
            .join(protocol::DEPLOYMENTS_DIR)
            .join(hex(&ID));
        let manifest = format!(
            "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n",
            hex(&digest(&kernel)),
            "00".repeat(32)
        );
        std::fs::write(directory.join(protocol::MANIFEST_NAME), &manifest).unwrap();
        std::fs::write(directory.join("bzImage"), &kernel).unwrap();
        let id = digest(manifest.as_bytes());
        let line = format!("{}\n", hex(&id));
        assert_eq!(
            authenticated_kernel(&directory, &id, line.as_bytes()).unwrap(),
            kernel
        );
        // Another line, or none.
        for other in [String::new(), hex(&id), format!("{}\n", hex(&ID))] {
            assert!(authenticated_kernel(&directory, &id, other.as_bytes()).is_err());
        }
        // A manifest that is not the ID.
        assert!(
            authenticated_kernel(&directory, &ID, format!("{}\n", hex(&ID)).as_bytes()).is_err()
        );
        // A kernel the manifest does not name.
        std::fs::write(directory.join("bzImage"), b"other kernel").unwrap();
        assert!(authenticated_kernel(&directory, &id, line.as_bytes())
            .is_err_and(|e| e.contains("does not match")));
    }

    /// The worker's td-boot children read a bounded amount: discovery and
    /// the manifest's authentication, never a whole deployment's hash, which
    /// a worker root killed would leave running.
    #[test]
    fn the_worker_runs_only_bounded_td_boot_verbs() {
        let source = include_str!("storage_operation.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert_eq!(production.matches("Command::new(TD_BOOT)").count(), 2);
        assert_eq!(production.matches("Command::new(").count(), 2);
        assert!(production.contains(".args([\"volume\", uuid])"));
        assert!(production.contains(".arg(\"authenticate\")"));
        for verb in ["validate-source", "verify", "boot", "install"] {
            assert!(!production.contains(&format!("\"{verb}\"")), "{verb}");
        }
    }

    /// The kernel's digest comes from the manifest's own form, once.
    #[test]
    fn the_manifest_names_its_kernel_once_in_its_own_form() {
        let manifest = format!(
            "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n{}  root.erofs\n",
            "ab".repeat(32),
            "cd".repeat(32),
            "ef".repeat(32)
        );
        assert_eq!(
            manifest_digest(manifest.as_bytes(), "bzImage"),
            Some([0xab; 32])
        );
        assert_eq!(manifest_digest(manifest.as_bytes(), "selector"), None);
        let other = manifest.replace("td-deployment-v1", "td-deployment-v2");
        assert_eq!(manifest_digest(other.as_bytes(), "bzImage"), None);
        let twice = format!("{manifest}{}  bzImage\n", "ab".repeat(32));
        assert_eq!(manifest_digest(twice.as_bytes(), "bzImage"), None);
        assert_eq!(id_bytes("AB".repeat(32).as_bytes()), None);
        assert_eq!(id_bytes("ab".repeat(31).as_bytes()), None);
        assert_eq!(id_bytes(&[b'a'; 65]), None);
    }

    #[test]
    fn failure_kinds_are_the_documented_bytes() {
        for (failure, frame) in [
            (Failure::WrongPin(5), &[0x15, 0x01, 5][..]),
            (Failure::PinAuthBlocked, &[0x15, 0x02]),
            (Failure::PinBlocked, &[0x15, 0x03]),
            (Failure::NotEnrolled, &[0x15, 0x04]),
            (Failure::OneKey, &[0x15, 0x05]),
            (
                Failure::Refused(LoginRefusal::NoHmacSecret),
                &[0x15, 0x06, 1],
            ),
            (Failure::Refused(LoginRefusal::AlwaysUv), &[0x15, 0x06, 2]),
            (
                Failure::Refused(LoginRefusal::PinUnsupported),
                &[0x15, 0x06, 3],
            ),
            (Failure::Refused(LoginRefusal::PinNotSet), &[0x15, 0x06, 4]),
            (
                Failure::Refused(LoginRefusal::ListTooSmall),
                &[0x15, 0x06, 5],
            ),
            (Failure::Refused(LoginRefusal::NoFido21), &[0x15, 0x0f]),
            (Failure::Refused(LoginRefusal::PinOptional), &[0x15, 0x0f]),
            (Failure::Denied, &[0x15, 0x07]),
            (Failure::Timeout, &[0x15, 0x08]),
            (Failure::Failed, &[0x15, 0x0f]),
            (Failure::Internal, &[0x15, 0x10]),
            (Failure::Volume, &[0x15, 0x20]),
            (Failure::Deployment, &[0x15, 0x23]),
            (Failure::Esp, &[0x15, 0x24]),
            (Failure::Mistyped, &[0x15, 0x25]),
            (Failure::WrongKey, &[0x15, 0x26]),
            (Failure::Changed, &[0x15, 0x28]),
            (Failure::Excluded, &[0x15, 0x11]),
            (Failure::Unprobed, &[0x15, 0x06, 6]),
            (Failure::State, &[0x15, 0x21]),
            (Failure::Tpm(TpmRefusal::Absent), &[0x15, 0x27, 1]),
            (Failure::Tpm(TpmRefusal::NoSha256Bank), &[0x15, 0x27, 2]),
            (Failure::Tpm(TpmRefusal::LockoutSet), &[0x15, 0x27, 3]),
        ] {
            assert_eq!(failure.frame(), frame, "{failure:?}");
        }
    }

    /// td-fido's fingerprint, the first four bytes of SHA-256, over FIPS
    /// 180-2's "abc" vector.
    #[test]
    fn the_fingerprint_is_the_first_four_bytes_of_sha256() {
        assert_eq!(fingerprint(b"abc"), [0xba, 0x78, 0x16, 0xbf]);
    }

    /// P-256's generator, a public key td-fido admits.
    const G_X: [u8; 32] = [
        0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4, 0x40,
        0xf2, 0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45, 0xd8, 0x98,
        0xc2, 0x96,
    ];
    const G_Y: [u8; 32] = [
        0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f, 0x9e,
        0x16, 0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68, 0x37, 0xbf,
        0x51, 0xf5,
    ];

    fn keyed(role: FidoRole, keyslot: Option<u8>, credential: u8) -> Token {
        Token::Fido2(Fido2 {
            keyslot,
            role,
            credential: vec![credential; 16],
            salt: [3; 32],
            x: G_X,
            y: G_Y,
        })
    }

    /// The FIDO2-primary shape with usable keys: the marker on keyslot 0,
    /// a primary key on 1 and a recovery key on 2.
    fn proof_header() -> Header {
        header(vec![
            (0, Token::RecoveryKey { keyslot: Some(0) }),
            (1, keyed(FidoRole::Primary, Some(1), 7)),
            (2, keyed(FidoRole::Recovery, Some(2), 8)),
        ])
    }

    /// An addition to a two-key login record, authorized by `proof`.
    fn adding(proof: Proof, step: LoginStep) -> Request {
        Request::new(
            NONCE,
            1000,
            Operation::LoginAdd {
                account: 1000,
                before: 2,
                after: 3,
                step,
                proof: Some(proof),
            },
        )
        .unwrap()
    }

    fn recovery_digits() -> Vec<u8> {
        RecoveryKey::generate()
            .unwrap()
            .passphrase()
            .expose()
            .to_vec()
    }

    fn prove(host: Fake, keys: &mut Scripted, request: &Request, secret: &[u8]) -> Run {
        proved(
            host,
            keys,
            Some(request.encode()),
            vec![secret.to_vec()],
            Duration::from_secs(5),
        )
    }

    #[test]
    fn a_recovery_key_proof_tests_the_markers_keyslot_and_writes_nothing() {
        let digits = recovery_digits();
        let mut keys = Scripted::selecting(0);
        let request = adding(Proof::RecoveryKey, LoginStep::RecoveryKey);
        let run = prove(
            Fake::new(Some(Ok(proof_header()))),
            &mut keys,
            &request,
            &digits,
        );
        assert_eq!(run.result, Ok(()));
        // The one step, presented exactly, then success: no commit round.
        assert_eq!(run.frames.len(), 3);
        assert_eq!(run.frames[1][0], PRESENT);
        assert_eq!(&run.frames[1][33..], &request.encode()[..]);
        assert_eq!(run.frames[2], [SUCCESS]);
        // The 48 digits alone reach the marker's keyslot.
        assert_eq!(run.tested, [(0, digits)]);
        bound_to_the_presented_header(&run);
        // The header again before the test; no kernel, ESP or key.
        assert_eq!(run.log, ["header", "standing", "header", "test"]);
        assert!(keys.deadline.is_none());
    }

    #[test]
    fn a_recovery_key_proof_ends_at_each_refusal_with_its_kind() {
        let digits = recovery_digits();
        let request = adding(Proof::RecoveryKey, LoginStep::RecoveryKey);
        // One wrong check digit: the codec refuses it before any re-read
        // or test, and no frame carries the entry.
        let mut mistyped = digits.clone();
        mistyped[5] = if mistyped[5] == b'9' {
            b'0'
        } else {
            mistyped[5] + 1
        };
        let run = prove(
            Fake::new(Some(Ok(proof_header()))),
            &mut Scripted::selecting(0),
            &request,
            &mistyped,
        );
        assert_eq!(run.result, Err(Failure::Mistyped));
        assert_eq!(run.frames.last().unwrap(), &[0x15, 0x25]);
        assert!(run.tested.is_empty());
        assert_eq!(run.log, ["header", "standing"]);
        for frame in &run.frames {
            assert!(!frame.windows(12).any(|window| window == &mistyped[..12]));
        }
        // The keyslot refuses the key, the copy is not the header
        // presented, cryptsetup outlasts the operation or fails otherwise.
        for (opens, result, kind) in [
            (Ok(Tested::Refused), Err(Failure::WrongKey), 0x26),
            (Ok(Tested::Changed), Ok(()), 0x28),
            (Ok(Tested::Expired), Err(Failure::Timeout), 0x08),
            (
                Err("cryptsetup failed".to_string()),
                Err(Failure::Failed),
                0x0f,
            ),
        ] {
            let mut host = Fake::new(Some(Ok(proof_header())));
            host.opens = opens;
            let run = prove(host, &mut Scripted::selecting(0), &request, &digits);
            assert_eq!(run.result, result);
            assert_eq!(run.frames.last().unwrap(), &[0x15, kind]);
            assert_eq!(run.tested.len(), 1);
        }
        // A header that changed since the baseline is definite and tests
        // nothing; one gone or refused now is `20`.
        let mut changed = proof_header();
        changed.seqid = 2;
        for (now, result, kind) in [
            (Some(Ok(changed)), Ok(()), 0x28),
            (None, Err(Failure::Volume), 0x20),
            (Some(Err("refused".to_string())), Err(Failure::Volume), 0x20),
        ] {
            let mut host = Fake::new(Some(Ok(proof_header())));
            host.reads.push(now);
            let run = prove(host, &mut Scripted::selecting(0), &request, &digits);
            assert_eq!(run.result, result);
            assert_eq!(run.frames.last().unwrap(), &[0x15, kind]);
            assert!(run.tested.is_empty());
        }
    }

    /// Root's first frame must be a proof's first step the baseline
    /// offers, for the worker's account; anything else is INTERNAL before
    /// any presentation.
    #[test]
    fn a_proof_the_baseline_does_not_offer_presents_nothing() {
        let unmarked = header(vec![(1, keyed(FidoRole::Primary, Some(1), 7))]);
        let other = Request::new(
            NONCE,
            1001,
            Operation::LoginAdd {
                account: 1001,
                before: 2,
                after: 3,
                step: LoginStep::RecoveryKey,
                proof: Some(Proof::RecoveryKey),
            },
        )
        .unwrap();
        let ordinary = Request::new(
            NONCE,
            1000,
            Operation::LoginAdd {
                account: 1000,
                before: 2,
                after: 3,
                step: LoginStep::Identify,
                proof: None,
            },
        )
        .unwrap();
        for (read, request) in [
            (
                Some(Ok(unmarked)),
                adding(Proof::RecoveryKey, LoginStep::RecoveryKey),
            ),
            (
                Some(Ok(tpm_header())),
                adding(Proof::DiskKey, LoginStep::DiskIdentify),
            ),
            (
                Some(Ok(proof_header())),
                adding(Proof::DiskKey, LoginStep::Connect),
            ),
            (
                Some(Ok(proof_header())),
                adding(
                    Proof::DiskKey,
                    LoginStep::DiskAuthorize {
                        key: fingerprint(&[7; 16]),
                        retries: 8,
                    },
                ),
            ),
            (Some(Ok(proof_header())), ordinary),
            (Some(Ok(proof_header())), other),
            (None, adding(Proof::RecoveryKey, LoginStep::RecoveryKey)),
            (
                Some(Err("refused".into())),
                adding(Proof::RecoveryKey, LoginStep::RecoveryKey),
            ),
        ] {
            let mut keys = Scripted::selecting(0);
            let run = prove(Fake::new(read), &mut keys, &request, b"1234");
            assert_eq!(run.result, Err(Failure::Internal), "{request:?}");
            assert_eq!(run.frames.len(), 2);
            assert_eq!(run.frames[1], [0x15, 0x10]);
            assert!(run.tested.is_empty());
            assert!(keys.deadline.is_none());
        }
    }

    #[test]
    fn a_disk_key_proof_presents_both_steps_and_tests_its_tokens_keyslot() {
        let mut keys = Scripted::selecting(1);
        let identify = adding(Proof::DiskKey, LoginStep::DiskIdentify);
        let before = Instant::now();
        let run = prove(
            Fake::new(Some(Ok(proof_header()))),
            &mut keys,
            &identify,
            b"1234",
        );
        assert_eq!(run.result, Ok(()));
        // Identify, then the authorize step naming the selected token and
        // the key's own retries, then success.
        let authorize = adding(
            Proof::DiskKey,
            LoginStep::DiskAuthorize {
                key: fingerprint(&[8; 16]),
                retries: 8,
            },
        );
        assert_eq!(run.frames.len(), 4);
        assert_eq!(&run.frames[1][33..], &identify.encode()[..]);
        assert_eq!(&run.frames[2][33..], &authorize.encode()[..]);
        assert_eq!(run.frames[3], [SUCCESS]);
        // td-fido binds identify to the first and the assertion to the
        // second.
        assert_eq!(keys.identify, identify.encode());
        assert_eq!(keys.authorize, Some(authorize.encode()));
        // That token's keyslot, with the passphrase of this volume and its
        // credential.
        let volume = volume_uuid(UUID).unwrap();
        let expected = fido_disk::passphrase(&OUTPUT, &volume, &[8; 16]).unwrap();
        assert_eq!(run.tested, [(2, expected.bytes().to_vec())]);
        assert_eq!(run.log, ["header", "standing", "header", "test"]);
        bound_to_the_presented_header(&run);
        // The key's sessions live one lifetime at most.
        let deadline = keys.deadline.unwrap();
        assert!(deadline <= Instant::now() + MAX_LIFETIME);
        assert!(deadline >= before + MAX_LIFETIME.min(Duration::from_secs(60)));
    }

    /// The test is of the header the baseline presented, its child bounded
    /// by the description's ceiling, an addition's 240 seconds.
    fn bound_to_the_presented_header(run: &Run) {
        let [(header, deadline)] = run.expected.as_slice() else {
            panic!("one keyslot test");
        };
        assert_eq!(*header, proof_header());
        assert!(*deadline <= Instant::now() + Duration::from_secs(240));
        assert!(*deadline > Instant::now() + Duration::from_secs(200));
    }

    /// A header token whose point td-fido refuses is not tried, as the
    /// selector's release skips it: the others still prove, each named by
    /// its own fingerprint and keyslot.
    #[test]
    fn a_token_with_an_unusable_point_is_skipped() {
        let mut header = proof_header();
        let Token::Fido2(primary) = &mut header.protected[1].1 else {
            panic!("the primary key");
        };
        primary.x = [0; 32];
        primary.y = [0; 32];
        let identify = adding(Proof::DiskKey, LoginStep::DiskIdentify);
        let mut keys = Scripted::selecting(0);
        let run = prove(Fake::new(Some(Ok(header))), &mut keys, &identify, b"1234");
        assert_eq!(run.result, Ok(()));
        let authorize = adding(
            Proof::DiskKey,
            LoginStep::DiskAuthorize {
                key: fingerprint(&[8; 16]),
                retries: 8,
            },
        );
        assert_eq!(keys.authorize, Some(authorize.encode()));
        let volume = volume_uuid(UUID).unwrap();
        let expected = fido_disk::passphrase(&OUTPUT, &volume, &[8; 16]).unwrap();
        assert_eq!(run.tested, [(2, expected.bytes().to_vec())]);
    }

    #[test]
    fn a_disk_key_proof_ends_with_the_login_workers_kinds() {
        let identify = adding(Proof::DiskKey, LoginStep::DiskIdentify);
        let status = |status| DiskError::Failed(FidoError::Status(status));
        for (before, after, frame, steps) in [
            (Some(DiskError::NotEnrolled), None, vec![0x15, 0x04], 1),
            (Some(DiskError::OneKey), None, vec![0x15, 0x05], 1),
            (
                Some(DiskError::Refused(LoginRefusal::NoHmacSecret)),
                None,
                vec![0x15, 0x06, 1],
                1,
            ),
            (
                Some(DiskError::Refused(LoginRefusal::NoFido21)),
                None,
                vec![0x15, 0x0f],
                1,
            ),
            // A wrong PIN before any step was asked is no PIN of this proof.
            (Some(status(Status::PinInvalid)), None, vec![0x15, 0x0f], 1),
            (Some(status(Status::PinBlocked)), None, vec![0x15, 0x03], 1),
            (
                None,
                Some(status(Status::PinInvalid)),
                vec![0x15, 0x01, 8],
                2,
            ),
            (
                None,
                Some(status(Status::PinAuthBlocked)),
                vec![0x15, 0x02],
                2,
            ),
            (None, Some(status(Status::Denied)), vec![0x15, 0x07], 2),
            // An exclusion is a creation's `11`, never a proof's.
            (
                None,
                Some(status(Status::CredentialExcluded)),
                vec![0x15, 0x0f],
                2,
            ),
            (
                None,
                Some(status(Status::TouchTimeout)),
                vec![0x15, 0x08],
                2,
            ),
            (
                None,
                Some(DiskError::Failed(FidoError::Interrupted(
                    Interruption::Expired,
                ))),
                vec![0x15, 0x08],
                2,
            ),
            (
                None,
                Some(DiskError::Failed(FidoError::Transport)),
                vec![0x15, 0x0f],
                2,
            ),
        ] {
            let mut keys = Scripted::selecting(1);
            keys.before = before;
            keys.after = after;
            let run = prove(
                Fake::new(Some(Ok(proof_header()))),
                &mut keys,
                &identify,
                b"1234",
            );
            assert_eq!(run.frames.last().unwrap(), &frame);
            assert_eq!(run.frames.len(), 2 + steps, "{frame:?}");
            assert!(run.tested.is_empty());
        }
        // A PIN outside its profile ends the step as INTERNAL.
        let run = prove(
            Fake::new(Some(Ok(proof_header()))),
            &mut Scripted::selecting(1),
            &identify,
            b"12",
        );
        assert_eq!(run.result, Err(Failure::Internal));
        assert_eq!(run.frames.last().unwrap(), &[0x15, 0x10]);
        // The keyslot refuses the passphrase: a token added to the header
        // whose keyslot this key does not open, or one naming none.
        let mut host = Fake::new(Some(Ok(proof_header())));
        host.opens = Ok(Tested::Refused);
        let run = prove(host, &mut Scripted::selecting(1), &identify, b"1234");
        assert_eq!(run.result, Err(Failure::WrongKey));
        assert_eq!(run.frames.last().unwrap(), &[0x15, 0x26]);
        let mut slotless = proof_header();
        slotless.protected[2].1 = keyed(FidoRole::Recovery, None, 8);
        let run = prove(
            Fake::new(Some(Ok(slotless))),
            &mut Scripted::selecting(1),
            &identify,
            b"1234",
        );
        assert_eq!(run.result, Err(Failure::WrongKey));
        assert!(run.tested.is_empty());
        // A header changed during the ceremony tests nothing.
        let mut changed = proof_header();
        changed.seqid = 2;
        let mut host = Fake::new(Some(Ok(proof_header())));
        host.reads.push(Some(Ok(changed)));
        let run = prove(host, &mut Scripted::selecting(1), &identify, b"1234");
        assert_eq!(run.frames.last().unwrap(), &[0x15, 0x28]);
        assert!(run.tested.is_empty());
    }

    /// The secret's path holds it only in storage zeroed on drop: the
    /// entry frame, the codec's key and its passphrase, and the PIN, none
    /// copied into an ordinary buffer, a string or a diagnostic.
    #[test]
    fn a_proofs_secret_paths_copy_no_secret() {
        let source = include_str!("storage_operation.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for (start, end) in [
            ("    fn recovery_key<", "    fn disk_key<"),
            (
                "    fn present(&mut self, token: usize",
                "impl Steps for Presenter",
            ),
            ("    fn pin_step(", "impl CreateSteps for Ceremony"),
            (
                "        let device = self.device.as_ref().ok_or(\"no held partition\")?;\n        let partition",
                "    fn upgrade_view(&mut self)",
            ),
        ] {
            let body = production
                .split(start)
                .nth(1)
                .unwrap()
                .split(end)
                .next()
                .unwrap();
            for copy in [
                "to_vec(",
                "to_owned(",
                "String::",
                "format!",
                ".clone()",
                "Vec::from",
                "collect",
                "from_utf8",
                "eprintln",
            ] {
                assert!(!body.contains(copy), "{start}: {copy}");
            }
        }
    }

    /// One LUKS2 header copy as cryptsetup lays it out, checksummed here.
    fn luks_copy(secondary: bool, seqid: u64, json: &str) -> Vec<u8> {
        let mut area = vec![0u8; 0x4000];
        area[..6].copy_from_slice(if secondary {
            b"SKUL\xba\xbe"
        } else {
            b"LUKS\xba\xbe"
        });
        area[6..8].copy_from_slice(&2u16.to_be_bytes());
        area[8..16].copy_from_slice(&0x4000u64.to_be_bytes());
        area[16..24].copy_from_slice(&seqid.to_be_bytes());
        area[24..33].copy_from_slice(b"td-system");
        area[72..78].copy_from_slice(b"sha256");
        area[104..168].fill(if secondary { 0x52 } else { 0x51 });
        area[168..168 + UUID.len()].copy_from_slice(UUID.as_bytes());
        let offset: u64 = if secondary { 0x4000 } else { 0 };
        area[256..264].copy_from_slice(&offset.to_be_bytes());
        area[4096..4096 + json.len()].copy_from_slice(json.as_bytes());
        let checksum = digest(&area);
        area[448..480].copy_from_slice(&checksum);
        area
    }

    /// The keyslots area the test headers declare.
    const KEYSLOTS_SIZE: usize = 8192;

    /// A partition whose primary copy is current and whose secondary is a
    /// stale one cryptsetup would repair, then its keyslots area and data:
    /// keyslot 0 has td's PBKDF2, keyslot 1 Argon2.
    fn partition(seqid: u64) -> Vec<u8> {
        let json = |seqid: u64| {
            format!(
                "{{\"keyslots\":{{\"0\":{{\"type\":\"luks2\",\"kdf\":{{\"type\":\"pbkdf2\",\"hash\":\"sha256\",\"iterations\":1000}}}},\"1\":{{\"type\":\"luks2\",\"kdf\":{{\"type\":\"argon2id\",\"time\":4,\"memory\":1048576,\"cpus\":4}}}}}},\"tokens\":{{}},\"segments\":{{\"0\":{{\"type\":\"crypt\",\"offset\":\"{seqid}\"}}}},\"digests\":{{\"0\":{{\"type\":\"pbkdf2\",\"keyslots\":[\"0\",\"1\"],\"hash\":\"sha256\",\"iterations\":250000}}}},\"config\":{{\"json_size\":\"12288\",\"keyslots_size\":\"{KEYSLOTS_SIZE}\"}}}}"
            )
        };
        let mut out = luks_copy(false, seqid, &json(seqid));
        out.extend(luks_copy(true, seqid - 1, &json(seqid - 1)));
        out.extend(vec![0x6b; KEYSLOTS_SIZE]);
        out.extend(vec![0xda; 4096]);
        out
    }

    /// Where a scripted cryptsetup writes what it saw, run by the machine
    /// over a held scratch partition.
    struct Keyslot {
        host: Rooted,
        bin: PathBuf,
        device: PathBuf,
        image: Vec<u8>,
        expected: Header,
    }

    fn tool(name: &str) -> PathBuf {
        std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(name))
                    .find(|candidate| candidate.is_file())
            })
            .unwrap()
    }

    fn keyslot() -> Keyslot {
        let mut host = rooted("");
        let bin = host.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(host.root.join(RUN_DIR)).unwrap();
        // It records its arguments, its input and the header it was given,
        // then repairs that header as cryptsetup repairs a stale copy.
        crate::scratch::executable(
            &bin.join(protocol::CRYPTSETUP),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{dir}/argv'\n\
                 h=\nfor a; do if [ \"$p\" = --header ]; then h=$a; fi; p=$a; done\n\
                 '{cat}' > '{dir}/stdin'\n'{cat}' \"$h\" > '{dir}/seen'\n\
                 printf 'REPAIRED' > \"$h\"\n\
                 read key < '{dir}/stdin' || :\n\
                 case \"$key\" in right) exit 0;; wrong) exit 2;; \
                 stall) echo $$ > '{dir}/pid'; exec '{sleep}' 60;; *) exit 1;; esac\n",
                dir = bin.display(),
                cat = tool("cat").display(),
                sleep = tool("sleep").display()
            ),
        )
        .unwrap();
        let image = partition(5);
        let device = host.root.join("partition");
        std::fs::write(&device, &image).unwrap();
        host.machine.partition = Some(File::open(&device).unwrap());
        host.machine.device = Some(device.clone());
        let expected = luks2::read(&mut std::io::Cursor::new(&image)).unwrap();
        Keyslot {
            host,
            bin,
            device,
            image,
            expected,
        }
    }

    impl Keyslot {
        fn test(&mut self, slot: u8, key: &[u8], deadline: Instant) -> Result<Tested, String> {
            let expected = self.expected.clone();
            self.host
                .machine
                .test_keyslot(slot, key, &expected, deadline)
        }

        fn ran(&self) -> bool {
            self.bin.join("argv").exists()
        }

        /// The disk as it was, and no copy left behind.
        fn untouched(&self) {
            assert_eq!(std::fs::read(&self.device).unwrap(), self.image);
            let copies = std::fs::read_dir(self.host.root.join(RUN_DIR))
                .unwrap()
                .filter(|entry| {
                    let name = entry.as_ref().unwrap().file_name();
                    name.to_string_lossy().starts_with("td-keyslot-test.")
                })
                .count();
            assert_eq!(copies, 0);
        }
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// A worker's start sweeps the copies killed workers left: only real
    /// directories root owns, named exactly for a pid no live process
    /// has. A live worker's copy, a stranger's, a symlink to a directory
    /// and any other name stay.
    #[test]
    fn a_start_sweeps_only_copies_dead_workers_left() {
        use std::os::unix::fs::MetadataExt as _;
        let run = crate::scratch::path("storage-sweep");
        std::fs::create_dir_all(&run).unwrap();
        let owner = std::fs::metadata(&run).unwrap().uid();
        let dir = |name: &str| {
            let path = run.join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("header"), b"copy").unwrap();
            path
        };
        let dead = [dir("td-keyslot-test.41"), dir("td-keyslot-test.43")];
        let kept = [
            dir("td-keyslot-test.42"),
            dir("td-keyslot-test.+44"),
            dir("td-keyslot-test.044"),
            dir("td-keyslot-test."),
            dir("td-other.45"),
        ];
        let target = dir("elsewhere");
        let link = run.join("td-keyslot-test.46");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let file = run.join("td-keyslot-test.47");
        std::fs::write(&file, b"not a directory").unwrap();
        let alive = |pid: u32| pid == 42;
        // Another owner's are not root's to sweep.
        sweep(&run, owner.wrapping_add(1), &alive);
        assert!(dead.iter().all(|path| path.exists()));
        sweep(&run, owner, &alive);
        assert!(dead.iter().all(|path| !path.exists()));
        assert!(kept.iter().all(|path| path.join("header").exists()));
        assert!(target.join("header").exists());
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert!(file.exists());
        // A missing run directory sweeps nothing and fails nothing.
        sweep(&run.join("missing"), owner, &alive);
        std::fs::remove_dir_all(&run).unwrap();
    }

    /// The installed cryptsetup tests the keyslot against a private copy
    /// of exactly the header area the reader verified, the device named for
    /// its data alone: the repair a stale second copy draws lands in the
    /// copy, and the disk is unchanged. The key is on standard input
    /// alone; a refusal is `Refused`, any other failure an error that
    /// names no key.
    #[test]
    fn the_machine_tests_a_keyslot_against_a_detached_header_copy() {
        let mut test = keyslot();
        assert_eq!(test.test(0, b"right", later()), Ok(Tested::Opened));
        let copy = test
            .host
            .root
            .join(RUN_DIR)
            .join(format!("td-keyslot-test.{}", std::process::id()))
            .join("header");
        let argv = std::fs::read_to_string(test.bin.join("argv")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            [
                "open",
                "--test-passphrase",
                "--type",
                "luks2",
                "--header",
                copy.to_str().unwrap(),
                "--key-slot",
                "0",
                "--key-file=-",
                test.device.to_str().unwrap()
            ]
        );
        assert_eq!(std::fs::read(test.bin.join("stdin")).unwrap(), b"right");
        let length = 2 * 0x4000 + KEYSLOTS_SIZE;
        assert_eq!(
            std::fs::read(test.bin.join("seen")).unwrap(),
            test.image[..length]
        );
        test.untouched();
        assert_eq!(test.test(0, b"wrong", later()), Ok(Tested::Refused));
        let error = test.test(0, b"broken", later()).unwrap_err();
        assert!(!error.contains("broken"), "{error}");
        test.untouched();
    }

    /// Nothing runs for a header other than the one presented, a keyslot
    /// whose cost td does not bound, or a node that is not the held one.
    #[test]
    fn a_keyslot_test_runs_only_on_the_presented_header_and_the_held_node() {
        let mut test = keyslot();
        test.expected.seqid = 6;
        assert_eq!(test.test(0, b"right", later()), Ok(Tested::Changed));
        assert!(!test.ran());
        test.expected.seqid = 5;
        // Argon2's memory and time are not run.
        assert!(test.test(1, b"right", later()).is_err());
        assert!(!test.ran());
        // The path `td-boot volume` named now names another node.
        let other = test.host.root.join("other");
        std::fs::write(&other, &test.image).unwrap();
        test.host.machine.device = Some(other);
        assert!(test.test(0, b"right", later()).is_err());
        assert!(!test.ran());
        test.untouched();
    }

    /// A cryptsetup that outlasts the operation, or whose worker root hung
    /// up on, is killed and reaped, and its copy removed.
    #[test]
    fn a_stalled_keyslot_test_is_killed_at_the_deadline_or_a_hangup() {
        let mut test = keyslot();
        let pid = |test: &Keyslot| {
            let pid = std::fs::read_to_string(test.bin.join("pid")).unwrap();
            std::fs::remove_file(test.bin.join("pid")).unwrap();
            PathBuf::from(format!("/proc/{}", pid.trim()))
        };
        let started = Instant::now();
        let deadline = started + Duration::from_millis(500);
        assert_eq!(test.test(0, b"stall", deadline), Ok(Tested::Expired));
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(!pid(&test).exists());
        test.untouched();
        let (authority, root) = UnixStream::pair().unwrap();
        test.host.machine.authority = Some(authority);
        let hang = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop(root);
        });
        let started = Instant::now();
        assert!(test.test(0, b"stall", later()).is_err());
        assert!(started.elapsed() < Duration::from_secs(30));
        hang.join().unwrap();
        assert!(!pid(&test).exists());
        test.untouched();
        // A root still there abandons nothing.
        let (authority, root) = UnixStream::pair().unwrap();
        test.host.machine.authority = Some(authority);
        assert_eq!(test.test(0, b"right", later()), Ok(Tested::Opened));
        drop(root);
    }
}
