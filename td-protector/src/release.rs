//! The selector's release order (td-install/ENCRYPTION.md "Device-bound
//! default", steps 1 to 5; DESIGN.md "Release orchestration"): try the td
//! tokens, run the first-boot transition's seal and verification, cap PCR
//! 12, and only then let cryptsetup read the header, run the confirmed
//! transition plan and name the keyslot and secret that open the volume.
//! Every TPM operation opens its own client through `Tpm`, every cryptsetup
//! command runs through `Runner`, and every secret is zeroed when dropped.

use crate::cryptsetup::{Cryptsetup, KeyFile};
use crate::luks2::{Header, MAX_TD_TOKENS};
use crate::token::{Role, Token};
use crate::transition::{self, Input, Refusal, Released as Classified, Step, Transition};
use crate::{
    cap, first_boot_policy, observe, release_policy, seal, unseal, CapError, ObserveError, Secret,
    UnsealError,
};
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::Path;
use td_tpm::{Client, Device, SealedObject, Transport};

/// The most td tokens one release tries: the reader's bound on td tokens,
/// so every one a header can carry.
pub const MAX_ATTEMPTS: usize = MAX_TD_TOKENS;

/// A TPM the release reaches: each operation opens a fresh client, so every
/// transient handle and session is flushed when it ends.
pub trait Tpm {
    type Transport: Transport;
    fn open(&mut self) -> Result<Client<Self::Transport>, String>;
}

/// The kernel resource manager, `/dev/tpmrm0`.
pub struct DeviceTpm;

impl Tpm for DeviceTpm {
    type Transport = Device;
    fn open(&mut self) -> Result<Client<Device>, String> {
        Ok(Client::new(Device::open()?))
    }
}

/// The cryptsetup commands the release runs, after the cap only.
pub trait Runner {
    /// One command with `input` on its standard input (`Cryptsetup::run`).
    fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()>;
    /// `luksDump --dump-json-metadata` of `device` (`Cryptsetup::metadata`).
    fn metadata(&mut self, device: &Path) -> io::Result<Vec<u8>>;
}

impl Runner for Cryptsetup {
    fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
        Cryptsetup::run(self, args, input)
    }
    fn metadata(&mut self, device: &Path) -> io::Result<Vec<u8>> {
        Cryptsetup::metadata(self, device)
    }
}

/// Unseal one td token on its own client: a first-boot token under the
/// literal first-boot policy, a device-bound one under PCRs 4 and 9 as they
/// read now and a literal-zero PCR 12, so that the TPM, not a local check,
/// answers whether it was sealed to this boot chain and whether the cap is
/// still open (DESIGN.md "Unseal outcomes").
pub fn unseal_token<T: Transport>(
    mut client: Client<T>,
    token: &Token,
) -> Result<Secret, UnsealError> {
    let policy = match token.role() {
        Role::FirstBoot => first_boot_policy(),
        Role::DeviceBound => release_policy(&mut client),
    }
    .map_err(UnsealError::Other)?;
    unseal(client, &policy, token.sealed())
}

/// Why the release ends in the recovery flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// No TPM device appeared within td-boot's wait: nothing released and
    /// the cap was skipped.
    NoTpm,
    /// td's reader refused the header (ENCRYPTION.md step 1).
    Header(String),
    /// PCR 12 was already non-zero, so nothing could release.
    AlreadyClosed,
    /// No td token released.
    NothingReleased,
    /// The first-boot protector alone released, and PCR 4 or PCR 9 is
    /// unmeasured: no device-bound protector is sealed.
    Unmeasured { pcr: u8 },
    /// The first-boot protector alone released, and the device-bound
    /// protector could not be sealed or did not verify.
    Transition(String),
    /// Every released secret's keyslot failed cryptsetup's test.
    NoKeyslotOpens,
    /// A device-bound protector released but its keyslot failed its test,
    /// and only the first-boot protector is left, with no device-bound
    /// protector sealed this boot.
    FirstBootFallback,
    /// The released tokens do not classify over the header.
    Refused(Refusal),
}

impl fmt::Display for Recovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTpm => f.write_str("no TPM device: nothing released, PCR 12 not capped"),
            Self::Header(error) => write!(f, "the LUKS2 header is refused: {error}"),
            Self::AlreadyClosed => f.write_str("PCR 12 was already capped: nothing released"),
            Self::NothingReleased => f.write_str("no td protector released"),
            Self::Unmeasured { pcr } => write!(
                f,
                "PCR {pcr} is unmeasured: no device-bound protector is sealed"
            ),
            Self::Transition(error) => write!(f, "the first-boot transition: {error}"),
            Self::NoKeyslotOpens => f.write_str("no released protector's keyslot opens"),
            Self::FirstBootFallback => f.write_str(
                "the released device-bound protector's keyslot does not open, \
                 and the first-boot protector alone binds no boot chain",
            ),
            Self::Refused(refusal) => write!(f, "{refusal}"),
        }
    }
}

/// What the release decided.
pub enum Outcome {
    /// Open the volume with `secret`, which cryptsetup tested on `keyslot`
    /// this boot.
    Released { keyslot: u8, secret: Secret },
    /// The recovery flow. `reseal_offerable` is ENCRYPTION.md's condition
    /// for offering the confirmed reseal: this boot's own cap closed PCR 12,
    /// and td read the header a reseal plans over.
    Recovery {
        reason: Recovery,
        reseal_offerable: bool,
    },
    /// The cap is uncertain or mismatched: every released secret is zeroed,
    /// and the selector refuses boot and halts until a platform reset.
    Halt { reason: CapError },
}

fn recovery(reason: Recovery, reseal_offerable: bool) -> Outcome {
    Outcome::Recovery {
        reason,
        reseal_offerable,
    }
}

/// The new device-bound protector of the first-boot transition, sealed and
/// verified before the cap.
struct NewProtector {
    sealed: SealedObject,
    secret: Secret,
}

/// Run the release order over `header`, the result of `luks2::read` on the
/// volume (step 1, which td-boot runs to decide on its TPM wait). `device`
/// names the partition to cryptsetup, `/proc/<pid>/fd/N` for the selector's
/// held descriptor. `tpm` is `None` when no TPM device appeared within
/// td-boot's wait. No cryptsetup command runs before the cap, and every
/// secret not returned is zeroed. It never falls back to plaintext.
pub fn release<T: Tpm, C: Runner>(
    header: &Result<Header, String>,
    device: &Path,
    tpm: Option<&mut T>,
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) -> Outcome {
    let Some(tpm) = tpm else {
        let reason = Recovery::NoTpm;
        console(&reason.to_string());
        return recovery(reason, false);
    };
    if let Err(error) = header {
        console(&format!("LUKS2 header refused: {error}"));
    }
    // Step 2: every td token, device-bound ones first.
    let mut released: Vec<(u8, Secret)> = Vec::new();
    let mut first_boot_alone = false;
    if let Ok(header) = header {
        for (number, token) in &header.tokens {
            if token.keyslot() == transition::RECOVERY_SLOT {
                console(&format!(
                    "td token {number} names the recovery keyslot 0: never released"
                ));
            }
        }
        for (number, token) in candidates(header) {
            let role = token.role().name();
            match tpm
                .open()
                .map_err(UnsealError::Other)
                .and_then(|client| unseal_token(client, token))
            {
                Ok(secret) => {
                    console(&format!("td token {number} ({role}) released"));
                    released.push((number, secret));
                }
                Err(error) => console(&format!("td token {number} ({role}): {error}")),
            }
        }
        first_boot_alone = !released.is_empty()
            && released
                .iter()
                .all(|(number, _)| role_of(header, *number) == Some(Role::FirstBoot));
    }
    // Step 3: only when the first-boot protector alone released.
    let new = first_boot_alone.then(|| new_protector(tpm));
    // Step 4: whether or not anything released.
    let capped = tpm
        .open()
        .map_err(CapError::Uncertain)
        .and_then(|mut client| cap(&mut client));
    if let Err(error) = capped {
        console(&error.to_string());
        drop(released);
        drop(new);
        return match error {
            CapError::AlreadyClosed => recovery(Recovery::AlreadyClosed, false),
            reason => Outcome::Halt { reason },
        };
    }
    console("PCR 12 release cap closed");
    let header = match header {
        Ok(header) => header,
        Err(error) => return recovery(Recovery::Header(error.clone()), false),
    };
    let new = match new {
        Some(Err(reason)) => {
            console(&reason.to_string());
            return recovery(reason, true);
        }
        Some(Ok(new)) => Some(new),
        None => None,
    };
    if released.is_empty() {
        let reason = Recovery::NothingReleased;
        console(&reason.to_string());
        return recovery(reason, true);
    }
    // Step 5: after the cap.
    let outcome = execute(header, device, released, new, cryptsetup, console);
    if let Outcome::Recovery { reason, .. } = &outcome {
        console(&reason.to_string());
    }
    outcome
}

/// The td tokens to try, device-bound before first-boot and by number,
/// never one naming keyslot 0, at most `MAX_ATTEMPTS`.
fn candidates(header: &Header) -> Vec<(u8, &Token)> {
    let mut tokens: Vec<(u8, &Token)> = header
        .tokens
        .iter()
        .filter(|(_, token)| token.keyslot() != transition::RECOVERY_SLOT)
        .map(|(number, token)| (*number, token))
        .collect();
    tokens.sort_by_key(|(number, token)| (token.role() != Role::DeviceBound, *number));
    tokens.truncate(MAX_ATTEMPTS);
    tokens
}

fn role_of(header: &Header, number: u8) -> Option<Role> {
    header
        .tokens
        .iter()
        .find(|(n, _)| *n == number)
        .map(|(_, token)| token.role())
}

fn keyslot_of(header: &Header, number: u8) -> Option<u8> {
    header
        .tokens
        .iter()
        .find(|(n, _)| *n == number)
        .map(|(_, token)| token.keyslot())
}

/// Seal a device-bound protector to the observed PCR 4 and PCR 9 values
/// and a literal-zero PCR 12, then unseal it once to verify it.
fn new_protector<T: Tpm>(tpm: &mut T) -> Result<NewProtector, Recovery> {
    let policy = tpm
        .open()
        .map_err(ObserveError::Tpm)
        .and_then(|mut client| observe(&mut client))
        .map_err(|error| match error {
            ObserveError::Unmeasured { pcr } => Recovery::Unmeasured { pcr },
            ObserveError::Tpm(error) => Recovery::Transition(format!("read PCRs 4 and 9: {error}")),
        })?;
    let secret = Secret::generate().map_err(Recovery::Transition)?;
    let sealed = tpm
        .open()
        .and_then(|client| seal(client, &policy, &secret))
        .map_err(|error| Recovery::Transition(format!("seal: {error}")))?;
    let verified = tpm
        .open()
        .map_err(UnsealError::Other)
        .and_then(|client| unseal(client, &policy, &sealed))
        .map_err(|error| Recovery::Transition(format!("verify: {error}")))?;
    if verified.expose() != secret.expose() {
        return Err(Recovery::Transition(
            "verify: the new protector unsealed another secret".into(),
        ));
    }
    Ok(NewProtector { sealed, secret })
}

/// Which key opens the volume once a plan stops.
enum Opens {
    /// The released secret, on its token's keyslot.
    Released,
    /// The new protector's, once its keyslot's test passed.
    New(u8),
}

/// How a plan, or the test that stands for an empty one, ended.
enum Run {
    /// The released keyslot failed its test: fall back.
    OpenerFailed,
    Opens(Opens),
}

/// Step 5: classify, plan, confirm against cryptsetup's metadata and run,
/// falling back to the next released token when the released keyslot fails
/// its test, and to recovery when none is left; never halting.
fn execute<C: Runner>(
    header: &Header,
    device: &Path,
    mut released: Vec<(u8, Secret)>,
    mut new: Option<NewProtector>,
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) -> Outcome {
    loop {
        let numbers: Vec<u8> = released.iter().map(|(number, _)| *number).collect();
        let classified = match transition::classify(header, &numbers) {
            Ok(classified) => classified,
            Err(refusal) => return recovery(Recovery::Refused(refusal), true),
        };
        let (opener, transition) = match classified {
            Classified::Nothing => return recovery(Recovery::NoKeyslotOpens, true),
            Classified::DeviceBound { token } => (token, Some(Transition::Keep { token })),
            Classified::FirstBoot { token } => match new.as_ref() {
                Some(new) => (
                    token,
                    Some(Transition::FirstBoot {
                        token,
                        sealed: new.sealed.clone(),
                    }),
                ),
                // Reached only by falling back from a device-bound keyslot
                // that failed its test, so nothing was sealed this boot:
                // opening with the first-boot protector would release to
                // PCR 12 alone on every later boot too.
                None => return recovery(Recovery::FirstBootFallback, true),
            },
        };
        let (Some(slot), Some(opener_secret)) = (
            keyslot_of(header, opener),
            released
                .iter()
                .find(|(number, _)| *number == opener)
                .map(|(_, secret)| secret),
        ) else {
            return recovery(
                Recovery::Refused(Refusal::UnknownToken { token: opener }),
                true,
            );
        };
        let steps = transition
            .and_then(|transition| {
                confirmed_steps(header, device, &transition, cryptsetup, console)
            })
            .unwrap_or_default();
        let key = Keys {
            opener: slot,
            released: opener_secret,
            new: new.as_ref(),
            new_slot: steps.iter().find_map(|step| match step {
                Step::AddKeyslot { slot, .. } => Some(*slot),
                _ => None,
            }),
        };
        let run = if steps.is_empty() {
            // No plan runs: the volume opens with the released secret, so
            // its keyslot is tested as the plan's first step would be.
            run_steps(&[Step::Test { slot }], device, &key, cryptsetup, console)
        } else {
            run_steps(&steps, device, &key, cryptsetup, console)
        };
        match run {
            Run::OpenerFailed => {
                console(&format!(
                    "keyslot {slot} of td token {opener} does not open: trying the next released token"
                ));
                // Dropping it zeroes it.
                released.retain(|(number, _)| *number != opener);
            }
            Run::Opens(Opens::New(keyslot)) => {
                return match new.take() {
                    Some(new) => Outcome::Released {
                        keyslot,
                        secret: new.secret,
                    },
                    None => recovery(Recovery::NoKeyslotOpens, true),
                };
            }
            Run::Opens(Opens::Released) => {
                // Every other released secret is dropped, and zeroed, here.
                return match released.into_iter().find(|(number, _)| *number == opener) {
                    Some((_, secret)) => Outcome::Released {
                        keyslot: slot,
                        secret,
                    },
                    None => recovery(Recovery::NoKeyslotOpens, true),
                };
            }
        }
    }
}

/// The plan's steps once cryptsetup's own metadata confirms the header it
/// was computed from; `None` when no plan runs this boot.
fn confirmed_steps<C: Runner>(
    header: &Header,
    device: &Path,
    transition: &Transition,
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) -> Option<Vec<Step>> {
    let plan = match transition::plan(header, transition) {
        Ok(plan) => plan,
        Err(refusal) => {
            console(&format!("no transition this boot: {refusal}"));
            return None;
        }
    };
    if plan.is_empty() {
        return Some(Vec::new());
    }
    let metadata = match cryptsetup.metadata(device) {
        Ok(metadata) => metadata,
        Err(error) => {
            console(&format!("no transition this boot: {error}"));
            return None;
        }
    };
    match plan.confirm(&metadata) {
        Ok(steps) => Some(steps),
        Err(refusal) => {
            console(&format!("no transition this boot: {refusal}"));
            None
        }
    }
}

/// The keys a plan's steps may name: the released secret's keyslot and the
/// new protector's. Keyslot 0's recovery key is never among them here.
struct Keys<'a> {
    opener: u8,
    released: &'a Secret,
    new: Option<&'a NewProtector>,
    new_slot: Option<u8>,
}

impl Keys<'_> {
    fn of(&self, slot: u8) -> io::Result<&[u8]> {
        let secret = if slot == self.opener {
            Some(self.released)
        } else if Some(slot) == self.new_slot {
            self.new.map(|new| &new.secret)
        } else {
            None
        };
        secret
            .map(|secret| secret.expose().as_slice())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("no key for keyslot {slot}"),
                )
            })
    }
}

fn describe(step: &Step) -> String {
    match step {
        Step::Test { slot } => format!("keyslot {slot} tested"),
        Step::KillSlot { slot, by } => {
            format!("keyslot {slot} destroyed, keyslot {by} authorizing")
        }
        Step::RemoveToken { token } => format!("token {token} removed"),
        Step::AddKeyslot { slot, by } => format!("keyslot {slot} added, keyslot {by} authorizing"),
        Step::ImportToken { token } => format!("token for keyslot {} imported", token.keyslot()),
    }
}

/// Run `steps` in order, reporting each as it commits. A failed first test
/// of the released keyslot falls back; any other failure stops the plan,
/// and the volume opens with the key that is known to: the released one,
/// or the new protector's once its own test passed. What the plan leaves
/// over the next boot's plan removes.
fn run_steps<C: Runner>(
    steps: &[Step],
    device: &Path,
    keys: &Keys<'_>,
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) -> Run {
    let mut opens = Opens::Released;
    for (index, step) in steps.iter().enumerate() {
        let count = steps.len();
        match run_step(step, device, keys, cryptsetup) {
            Ok(()) => {
                console(&format!(
                    "transition step {}/{count}: {}",
                    index + 1,
                    describe(step)
                ));
                if let Step::Test { slot } = step {
                    if Some(*slot) == keys.new_slot {
                        opens = Opens::New(*slot);
                    }
                }
            }
            Err(error) => {
                console(&format!(
                    "transition step {}/{count} failed ({}): {error}",
                    index + 1,
                    describe(step)
                ));
                if index == 0 && *step == (Step::Test { slot: keys.opener }) {
                    return Run::OpenerFailed;
                }
                return Run::Opens(opens);
            }
        }
    }
    Run::Opens(opens)
}

/// One step through the runner: a key on standard input by the keyslot it
/// opens, the new token's JSON, or nothing; the new secret of `AddKeyslot`
/// as its key file, a descriptor this process holds.
fn run_step<C: Runner>(
    step: &Step,
    device: &Path,
    keys: &Keys<'_>,
    cryptsetup: &mut C,
) -> io::Result<()> {
    let json;
    let input: &[u8] = match step.input() {
        Input::KeyOf(slot) => keys.of(slot)?,
        Input::Token(token) => {
            json = token.encode();
            json.as_bytes()
        }
        Input::Nothing => &[],
    };
    if let Step::AddKeyslot { .. } = step {
        let new = keys.new.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no new protector to add")
        })?;
        let file = KeyFile::new(new.secret.expose())?;
        return cryptsetup.run(&step.args(device, file.path()), input);
    }
    // Only `AddKeyslot`'s arguments name a key file.
    cryptsetup.run(&step.args(device, Path::new("")), input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cryptsetup::MIN_KILL_KEY;
    use crate::luks2::HeaderCopy;
    use crate::tests::{live_secrets, Scripted};
    use crate::{cap_event, observed_policy, CAP_PCR};
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::io::Read;
    use td_tpm::{CREATE, PCR_EXTEND, PCR_READ, UNSEAL};

    impl Tpm for Scripted {
        type Transport = Scripted;
        fn open(&mut self) -> Result<Client<Scripted>, String> {
            Ok(self.client())
        }
    }

    const DEVICE: &str = "/proc/1/fd/7";
    /// Keyslot 0's passphrase: 48 recovery-key digits.
    const RECOVERY_KEY: &[u8] = b"123450123450123450123450123450123450123450123450";

    /// PCR 12 once the cap closed it.
    fn capped() -> [u8; 32] {
        td_tpm::digest(&[[0; 32], cap_event()].concat())
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Entry {
        Td(Token),
        Orphan(Role),
    }

    /// The keyslots and each token as (number, keyslot, role).
    type Shape = (Vec<u8>, Vec<(u8, Option<u8>, Role)>);

    /// A LUKS2 header as cryptsetup keeps it, and the cryptsetup commands
    /// run on it: each keyslot's key, a dead keyslot's area wiped, and the
    /// tokens. Every command first requires PCR 12 capped.
    struct Disk {
        keyslots: BTreeMap<u8, Vec<u8>>,
        dead: Vec<u8>,
        tokens: BTreeMap<u8, Entry>,
        calls: Vec<String>,
        /// Another header's metadata, as from a copy td did not read.
        metadata: Option<Vec<u8>>,
        /// Verbs that fail as cryptsetup would.
        fail: Vec<&'static str>,
        /// PCR 12 as each command finds it.
        pcr12: Box<dyn FnMut() -> [u8; 32]>,
        /// The TPM's command count, and its value at the first cryptsetup
        /// command.
        tpm_commands: Option<Box<dyn Fn() -> usize>>,
        tpm_commands_at_first: Option<usize>,
    }

    impl Disk {
        fn new(tpm: &Scripted) -> Self {
            let read = tpm.clone();
            let count = tpm.clone();
            Self {
                keyslots: BTreeMap::from([(0, RECOVERY_KEY.to_vec())]),
                dead: Vec::new(),
                tokens: BTreeMap::new(),
                calls: Vec::new(),
                metadata: None,
                fail: Vec::new(),
                pcr12: Box::new(move || read.pcr(usize::from(CAP_PCR))),
                tpm_commands: Some(Box::new(move || count.0.borrow().codes.len())),
                tpm_commands_at_first: None,
            }
        }

        /// A protector of `role` sealed by `tpm`, enrolled as token
        /// `number` on keyslot `slot`; its secret is returned.
        fn enroll<T: Tpm>(&mut self, tpm: &mut T, number: u8, slot: u8, role: Role) -> Secret {
            let policy = match role {
                Role::FirstBoot => first_boot_policy().unwrap(),
                Role::DeviceBound => observed_policy(&mut tpm.open().unwrap()).unwrap(),
            };
            let secret = Secret::generate().unwrap();
            let sealed = seal(tpm.open().unwrap(), &policy, &secret).unwrap();
            self.keyslots.insert(slot, secret.expose().to_vec());
            self.tokens
                .insert(number, Entry::Td(Token::new(slot, role, sealed).unwrap()));
            secret
        }

        fn header(&self) -> Header {
            let mut tokens = Vec::new();
            let mut orphans = Vec::new();
            for (number, entry) in &self.tokens {
                match entry {
                    Entry::Td(token) => tokens.push((*number, token.clone())),
                    Entry::Orphan(role) => orphans.push((*number, *role)),
                }
            }
            Header {
                copy: HeaderCopy::Primary,
                seqid: 1,
                hdr_size: 0x4000,
                uuid: "0f0e0d0c-0b0a-4908-8706-050403020100".into(),
                label: "td-system".into(),
                keyslots: self.keyslots.keys().copied().collect(),
                tokens,
                orphans,
                foreign: Vec::new(),
            }
        }

        /// The header as `luksDump --dump-json-metadata` prints it, with
        /// `extra` token entries.
        fn dump_with(&self, extra: &[String]) -> Vec<u8> {
            let keyslots: Vec<String> = self
                .keyslots
                .keys()
                .map(|slot| format!("\"{slot}\": {{\n      \"type\": \"luks2\"\n    }}"))
                .collect();
            let mut tokens: Vec<String> = self
                .tokens
                .iter()
                .map(|(number, entry)| match entry {
                    Entry::Td(token) => format!("\"{number}\": {}", token.encode()),
                    Entry::Orphan(role) => format!(
                        "\"{number}\": {{\"type\":\"td-protector\",\"keyslots\":[],\"role\":\"{}\",\"public\":\"00\",\"private\":\"00\"}}",
                        role.name()
                    ),
                })
                .collect();
            tokens.extend_from_slice(extra);
            format!(
                "{{\n  \"keyslots\": {{\n    {}\n  }},\n  \"tokens\": {{\n    {}\n  }},\n  \"segments\": {{}},\n  \"digests\": {{}},\n  \"config\": {{}}\n}}",
                keyslots.join(",\n    "),
                tokens.join(",\n    ")
            )
            .into_bytes()
        }

        fn opens(&self, slot: u8, key: &[u8]) -> bool {
            !self.dead.contains(&slot) && self.keyslots.get(&slot).is_some_and(|k| k == key)
        }

        fn refuse(&self, verb: &str, call: &str) -> io::Result<()> {
            if self.fail.contains(&verb) {
                return Err(io::Error::other(format!("cryptsetup {call} failed")));
            }
            Ok(())
        }

        fn shape(&self) -> Shape {
            (
                self.keyslots.keys().copied().collect(),
                self.tokens
                    .iter()
                    .map(|(number, entry)| match entry {
                        Entry::Td(token) => (*number, Some(token.keyslot()), token.role()),
                        Entry::Orphan(role) => (*number, None, *role),
                    })
                    .collect(),
            )
        }

        fn before_each_call(&mut self) {
            assert_eq!(
                (self.pcr12)(),
                capped(),
                "a cryptsetup command ran before the cap"
            );
            if self.tpm_commands_at_first.is_none() {
                self.tpm_commands_at_first = self.tpm_commands.as_ref().map(|count| count());
            }
        }
    }

    impl Runner for Disk {
        fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
            self.before_each_call();
            let words: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
            assert!(words.contains(&DEVICE), "{words:?}");
            let number = |at: usize| -> u8 { words[at].parse().unwrap() };
            match words[0] {
                "open" => {
                    assert_eq!(words[1], "--test-passphrase");
                    let slot = number(5);
                    let call = format!("test {slot}");
                    self.calls.push(call.clone());
                    self.refuse("test", &call)?;
                    if !self.opens(slot, input) {
                        return Err(io::Error::other(format!("keyslot {slot} does not open")));
                    }
                }
                "luksAddKey" => {
                    let (by, slot) = (number(9), number(11));
                    let call = format!("add {slot} by {by}");
                    self.calls.push(call.clone());
                    // The new key travels by descriptor alone.
                    let mut key = Vec::new();
                    File::open(words[14])
                        .unwrap()
                        .read_to_end(&mut key)
                        .unwrap();
                    assert_eq!(key.len(), crate::SECRET_LEN);
                    assert!(!args.iter().any(|arg| arg.as_encoded_bytes() == &key[..]));
                    self.refuse("add", &call)?;
                    assert!(self.opens(by, input), "{call}");
                    assert!(!self.keyslots.contains_key(&slot), "{call}");
                    self.keyslots.insert(slot, key);
                }
                "token" if words[1] == "import" => {
                    let token = Token::decode(input).unwrap();
                    let call = format!("import {}", token.keyslot());
                    self.calls.push(call.clone());
                    self.refuse("import", &call)?;
                    assert!(self.keyslots.contains_key(&token.keyslot()));
                    let free = (0..=crate::token::MAX_SLOT)
                        .find(|n| !self.tokens.contains_key(n))
                        .unwrap();
                    self.tokens.insert(free, Entry::Td(token));
                }
                "token" if words[1] == "remove" => {
                    let token = number(3);
                    let call = format!("remove {token}");
                    self.calls.push(call.clone());
                    self.refuse("remove", &call)?;
                    assert!(self.tokens.remove(&token).is_some(), "{call}");
                }
                "luksKillSlot" => {
                    let slot = number(4);
                    let call = format!("kill {slot}");
                    self.calls.push(call.clone());
                    // The runner's floor: never an empty or short key.
                    assert!(input.len() >= MIN_KILL_KEY, "{call}");
                    self.refuse("kill", &call)?;
                    assert!(slot != 0 && self.keyslots.contains_key(&slot), "{call}");
                    // cryptsetup tries the key on every other keyslot.
                    assert!(
                        self.keyslots
                            .keys()
                            .any(|other| *other != slot && self.opens(*other, input)),
                        "{call} unauthorized"
                    );
                    self.keyslots.remove(&slot);
                    self.dead.retain(|dead| *dead != slot);
                    for entry in self.tokens.values_mut() {
                        if let Entry::Td(token) = entry {
                            if token.keyslot() == slot {
                                *entry = Entry::Orphan(token.role());
                            }
                        }
                    }
                }
                verb => panic!("unscripted cryptsetup {verb}"),
            }
            Ok(())
        }

        fn metadata(&mut self, device: &Path) -> io::Result<Vec<u8>> {
            self.before_each_call();
            assert_eq!(device, Path::new(DEVICE));
            self.calls.push("dump".into());
            self.refuse("dump", "dump")?;
            Ok(self.metadata.clone().unwrap_or_else(|| self.dump_with(&[])))
        }
    }

    /// One boot's release over the disk, with the console's lines. Every
    /// secret it does not return is gone, and no TPM command follows the
    /// first cryptsetup command.
    fn boot<T: Tpm>(disk: &mut Disk, tpm: &mut T) -> (Outcome, Vec<String>) {
        boot_over(&Ok(disk.header()), disk, Some(tpm))
    }

    fn boot_over<T: Tpm>(
        header: &Result<Header, String>,
        disk: &mut Disk,
        tpm: Option<&mut T>,
    ) -> (Outcome, Vec<String>) {
        disk.calls.clear();
        disk.tpm_commands_at_first = None;
        let mut lines = Vec::new();
        let before = live_secrets();
        let outcome = release(header, Path::new(DEVICE), tpm, disk, &mut |line| {
            lines.push(line.to_owned())
        });
        let held = isize::from(matches!(outcome, Outcome::Released { .. }));
        assert_eq!(
            live_secrets() - before,
            held,
            "secrets left alive: {lines:?}"
        );
        if let (Some(at), Some(count)) = (disk.tpm_commands_at_first, &disk.tpm_commands) {
            assert_eq!(count(), at, "a TPM command after cryptsetup ran");
        }
        (outcome, lines)
    }

    fn released(outcome: Outcome, lines: &[String]) -> (u8, Secret) {
        match outcome {
            Outcome::Released { keyslot, secret } => (keyslot, secret),
            Outcome::Recovery { reason, .. } => panic!("recovery: {reason}: {lines:?}"),
            Outcome::Halt { reason } => panic!("halt: {reason}: {lines:?}"),
        }
    }

    fn recovered(outcome: Outcome, lines: &[String]) -> (Recovery, bool) {
        match outcome {
            Outcome::Recovery {
                reason,
                reseal_offerable,
            } => (reason, reseal_offerable),
            Outcome::Released { keyslot, .. } => panic!("released keyslot {keyslot}: {lines:?}"),
            Outcome::Halt { reason } => panic!("halt: {reason}: {lines:?}"),
        }
    }

    /// A platform reset: PCR 12 back at zero.
    fn reboot(tpm: &Scripted) {
        tpm.0.borrow_mut().pcrs[usize::from(CAP_PCR)] = [0; 32];
        tpm.codes();
    }

    fn count(codes: &[u32], code: u32) -> usize {
        codes.iter().filter(|c| **c == code).count()
    }

    /// The installed header: keyslot 0 and the first-boot protector.
    fn installed(tpm: &mut Scripted) -> (Disk, Secret) {
        let mut disk = Disk::new(tpm);
        let first = disk.enroll(tpm, 0, 1, Role::FirstBoot);
        (disk, first)
    }

    #[test]
    fn a_device_bound_protector_releases_and_retires_what_it_supersedes() {
        let mut tpm = Scripted::new();
        let (mut disk, _first) = installed(&mut tpm);
        let bound = disk.enroll(&mut tpm, 1, 2, Role::DeviceBound);
        // An orphan keyslot no token names.
        disk.keyslots.insert(3, vec![3; 32]);
        tpm.codes();
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, secret) = released(outcome, &lines);
        assert_eq!((keyslot, secret.expose()), (2, bound.expose()));
        assert_eq!(
            disk.calls,
            ["dump", "test 2", "kill 3", "kill 1", "remove 0"]
        );
        assert_eq!(
            disk.shape(),
            (vec![0, 2], vec![(1, Some(2), Role::DeviceBound)])
        );
        // Device-bound first, then first-boot: no seal, one cap.
        let codes = tpm.codes();
        assert_eq!(count(&codes, UNSEAL), 2);
        assert_eq!(count(&codes, CREATE), 0);
        assert_eq!(count(&codes, PCR_EXTEND), 1);
        assert!(lines
            .iter()
            .any(|l| l == "td token 1 (device-bound) released"));
        assert!(lines
            .iter()
            .any(|l| l == "transition step 4/4: token 0 removed"));

        // The next boot tests the kept keyslot and changes nothing.
        reboot(&tpm);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, secret) = released(outcome, &lines);
        assert_eq!((keyslot, secret.expose()), (2, bound.expose()));
        assert_eq!(disk.calls, ["test 2"]);
    }

    #[test]
    fn the_first_boot_protector_alone_seals_verifies_caps_and_transitions() {
        let mut tpm = Scripted::new();
        let (mut disk, _first) = installed(&mut tpm);
        tpm.codes();
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, new) = released(outcome, &lines);
        assert_eq!(keyslot, 2);
        assert_eq!(disk.keyslots[&2], new.expose());
        assert_eq!(
            disk.calls,
            [
                "dump",
                "test 1",
                "add 2 by 1",
                "import 2",
                "test 2",
                "kill 1",
                "remove 0"
            ]
        );
        assert_eq!(
            disk.shape(),
            (vec![0, 2], vec![(1, Some(2), Role::DeviceBound)])
        );
        // The first-boot unseal, the seal to PCRs 4 and 9, its verifying
        // unseal, then the cap last.
        let codes = tpm.codes();
        assert_eq!(count(&codes, UNSEAL), 2);
        assert_eq!(count(&codes, CREATE), 1);
        assert_eq!(codes[codes.len() - 3..], [PCR_READ, PCR_EXTEND, PCR_READ]);
        let create = codes.iter().position(|c| *c == CREATE).unwrap();
        let verify = codes.iter().rposition(|c| *c == UNSEAL).unwrap();
        assert!(create < verify && verify < codes.len() - 3);
        // Each commit is reported as it happens.
        for step in 1..=6 {
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with(&format!("transition step {step}/6: "))),
                "{lines:?}"
            );
        }

        // The next boot releases the new protector and keeps it.
        reboot(&tpm);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, secret) = released(outcome, &lines);
        assert_eq!((keyslot, secret.expose()), (2, new.expose()));
        assert_eq!(disk.calls, ["test 2"]);
        assert_eq!(count(&tpm.codes(), CREATE), 0);
    }

    #[test]
    fn nothing_released_reaches_recovery_without_cryptsetup() {
        let mut tpm = Scripted::new();
        let mut disk = Disk::new(&tpm);
        let _bound = disk.enroll(&mut tpm, 0, 1, Role::DeviceBound);
        // A changed selector image: the TPM refuses the policy.
        tpm.0.borrow_mut().pcrs[4] = [0x45; 32];
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        assert_eq!(
            recovered(outcome, &lines),
            (Recovery::NothingReleased, true)
        );
        assert!(disk.calls.is_empty());
        assert_eq!(tpm.pcr(12), capped());
        assert!(lines
            .iter()
            .any(|l| l.starts_with("td token 0 (device-bound): policy refused: ")));
    }

    #[test]
    fn an_already_closed_cap_reaches_recovery_with_no_plan() {
        let mut tpm = Scripted::new();
        let (mut disk, _first) = installed(&mut tpm);
        let _bound = disk.enroll(&mut tpm, 1, 2, Role::DeviceBound);
        tpm.0.borrow_mut().pcrs[12] = [1; 32];
        tpm.codes();
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        assert_eq!(recovered(outcome, &lines), (Recovery::AlreadyClosed, false));
        assert!(disk.calls.is_empty());
        let codes = tpm.codes();
        assert_eq!(count(&codes, PCR_EXTEND), 0);
        assert_eq!(count(&codes, UNSEAL), 0, "PolicyPCR refused both");
    }

    #[test]
    fn an_uncertain_or_mismatched_cap_halts_with_every_secret_zeroed() {
        for first_boot_alone in [false, true] {
            for skew in [false, true] {
                let mut tpm = Scripted::new();
                let (mut disk, _first) = installed(&mut tpm);
                if !first_boot_alone {
                    let _bound = disk.enroll(&mut tpm, 1, 2, Role::DeviceBound);
                }
                if skew {
                    tpm.0.borrow_mut().skew_extend = true;
                } else {
                    tpm.0.borrow_mut().refuse_extend = true;
                }
                // `boot` holds that no secret, released or new, outlives it.
                let (outcome, lines) = boot(&mut disk, &mut tpm);
                let Outcome::Halt { reason } = outcome else {
                    panic!("no halt: {lines:?}")
                };
                if skew {
                    assert_eq!(reason, CapError::Mismatch);
                } else {
                    assert!(matches!(reason, CapError::Uncertain(_)), "{reason}");
                }
                assert!(disk.calls.is_empty());
                assert!(lines.iter().any(|l| l.ends_with(" released")), "{lines:?}");
            }
        }
    }

    #[test]
    fn an_unmeasured_selector_pcr_refuses_the_seal_and_reaches_recovery() {
        for pcr in [4, 9] {
            let mut tpm = Scripted::new();
            let (mut disk, _first) = installed(&mut tpm);
            tpm.0.borrow_mut().pcrs[usize::from(pcr)] = [0; 32];
            tpm.codes();
            let (outcome, lines) = boot(&mut disk, &mut tpm);
            assert_eq!(
                recovered(outcome, &lines),
                (Recovery::Unmeasured { pcr }, true)
            );
            assert!(disk.calls.is_empty());
            let codes = tpm.codes();
            assert_eq!(count(&codes, CREATE), 0);
            assert_eq!(count(&codes, PCR_EXTEND), 1);
        }
    }

    #[test]
    fn a_cleared_tpm_loads_nothing_and_reaches_recovery() {
        let mut tpm = Scripted::new();
        let (mut disk, _first) = installed(&mut tpm);
        let _bound = disk.enroll(&mut tpm, 1, 2, Role::DeviceBound);
        tpm.0.borrow_mut().cleared = true;
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        assert_eq!(
            recovered(outcome, &lines),
            (Recovery::NothingReleased, true)
        );
        assert!(disk.calls.is_empty());
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.ends_with(": load refused: TPM command 0x157 refused: 0x1df"))
                .count(),
            2,
            "{lines:?}"
        );
    }

    #[test]
    fn metadata_that_disagrees_or_fails_opens_without_a_plan() {
        for fail in [false, true] {
            let mut tpm = Scripted::new();
            let (mut disk, _first) = installed(&mut tpm);
            let bound = disk.enroll(&mut tpm, 1, 2, Role::DeviceBound);
            if fail {
                disk.fail.push("dump");
            } else {
                disk.metadata =
                    Some(disk.dump_with(&[
                        "\"5\": {\"type\": \"systemd-tpm2\", \"keyslots\": []}".into(),
                    ]));
            }
            let before = disk.shape();
            let (outcome, lines) = boot(&mut disk, &mut tpm);
            let (keyslot, secret) = released(outcome, &lines);
            assert_eq!((keyslot, secret.expose()), (2, bound.expose()));
            assert_eq!(disk.calls, ["dump", "test 2"]);
            assert_eq!(disk.shape(), before);
            assert!(lines
                .iter()
                .any(|l| l.starts_with("no transition this boot: ")));
        }
    }

    #[test]
    fn a_dead_released_keyslot_falls_back_and_never_halts() {
        // Two device-bound protectors, the lower one's keyslot dead: the
        // other is kept and retires it.
        let mut tpm = Scripted::new();
        let mut disk = Disk::new(&tpm);
        let _dead = disk.enroll(&mut tpm, 0, 1, Role::DeviceBound);
        let live = disk.enroll(&mut tpm, 1, 2, Role::DeviceBound);
        disk.dead.push(1);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, secret) = released(outcome, &lines);
        assert_eq!((keyslot, secret.expose()), (2, live.expose()));
        assert_eq!(
            disk.calls,
            ["dump", "test 1", "dump", "test 2", "kill 1", "remove 0"]
        );
        assert_eq!(
            disk.shape(),
            (vec![0, 2], vec![(1, Some(2), Role::DeviceBound)])
        );

        // The first-boot protector alone, its keyslot dead: the sealed new
        // protector is never added, and recovery follows.
        let mut tpm = Scripted::new();
        let (mut disk, _first) = installed(&mut tpm);
        disk.dead.push(1);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        assert_eq!(recovered(outcome, &lines), (Recovery::NoKeyslotOpens, true));
        assert_eq!(disk.calls, ["dump", "test 1"]);
        assert_eq!(
            disk.shape(),
            (vec![0, 1], vec![(0, Some(1), Role::FirstBoot)])
        );

        // An empty plan's stand-in test finds it too.
        let mut tpm = Scripted::new();
        let mut disk = Disk::new(&tpm);
        let _dead = disk.enroll(&mut tpm, 0, 1, Role::DeviceBound);
        disk.dead.push(1);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        assert_eq!(recovered(outcome, &lines), (Recovery::NoKeyslotOpens, true));
        assert_eq!(disk.calls, ["test 1"]);
    }

    /// A confirmed recovery reseal, as the recovery flow will run it after
    /// this boot's cap: the planner's steps with the recovery key on
    /// keyslot 0 and the new protector's on its own.
    fn reseal(disk: &mut Disk, tpm: &mut Scripted) -> (u8, Secret) {
        let policy = observed_policy(&mut tpm.client()).unwrap();
        let secret = Secret::generate().unwrap();
        let sealed = seal(tpm.client(), &policy, &secret).unwrap();
        let steps = transition::plan(&disk.header(), &Transition::Reseal { sealed })
            .unwrap()
            .confirm(&disk.dump_with(&[]))
            .unwrap();
        let slot = steps
            .iter()
            .find_map(|step| match step {
                Step::AddKeyslot { slot, .. } => Some(*slot),
                _ => None,
            })
            .unwrap();
        for step in &steps {
            let file = KeyFile::new(secret.expose()).unwrap();
            let json;
            let input: &[u8] = match step.input() {
                Input::KeyOf(0) => RECOVERY_KEY,
                Input::KeyOf(key) if key == slot => secret.expose(),
                Input::KeyOf(key) => panic!("no key for keyslot {key}"),
                Input::Token(token) => {
                    json = token.encode();
                    json.as_bytes()
                }
                Input::Nothing => &[],
            };
            disk.run(&step.args(Path::new(DEVICE), file.path()), input)
                .unwrap();
        }
        (slot, secret)
    }

    /// A device-bound protector whose keyslot is dead beside a live
    /// first-boot one: the fall-back never opens with the first-boot
    /// protector alone, which would bind no boot chain on every boot, but
    /// reaches recovery every boot until a confirmed reseal, after which
    /// the new protector releases.
    #[test]
    fn a_fall_back_to_the_first_boot_protector_alone_reaches_recovery() {
        let mut tpm = Scripted::new();
        let (mut disk, _first) = installed(&mut tpm);
        let _dead = disk.enroll(&mut tpm, 1, 2, Role::DeviceBound);
        disk.dead.push(2);
        let before = disk.shape();
        for _ in 0..3 {
            reboot(&tpm);
            let (outcome, lines) = boot(&mut disk, &mut tpm);
            assert_eq!(
                recovered(outcome, &lines),
                (Recovery::FirstBootFallback, true)
            );
            assert_eq!(disk.calls, ["dump", "test 2"]);
            assert_eq!(disk.shape(), before);
            // No protector is sealed: a device-bound one released.
            assert_eq!(count(&tpm.codes(), CREATE), 0);
        }
        let (slot, secret) = reseal(&mut disk, &mut tpm);
        assert_eq!(
            disk.shape(),
            (vec![0, slot], vec![(2, Some(slot), Role::DeviceBound)])
        );
        reboot(&tpm);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, released_secret) = released(outcome, &lines);
        assert_eq!((keyslot, released_secret.expose()), (slot, secret.expose()));
        assert_eq!(disk.calls, [format!("test {slot}")]);
    }

    #[test]
    fn a_td_token_naming_keyslot_0_is_never_released_and_suppresses_every_plan() {
        let mut tpm = Scripted::new();
        let mut disk = Disk::new(&tpm);
        // A device-bound protector naming keyslot 0, which would release.
        drop(disk.enroll(&mut tpm, 0, 0, Role::DeviceBound));
        disk.keyslots.insert(0, RECOVERY_KEY.to_vec());
        let bound = disk.enroll(&mut tpm, 1, 2, Role::DeviceBound);
        disk.keyslots.insert(3, vec![3; 32]);
        tpm.codes();
        let before = disk.shape();
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, secret) = released(outcome, &lines);
        assert_eq!((keyslot, secret.expose()), (2, bound.expose()));
        // No plan, so no metadata dump, and the orphan keyslot stays.
        assert_eq!(disk.calls, ["test 2"]);
        assert_eq!(disk.shape(), before);
        assert_eq!(count(&tpm.codes(), UNSEAL), 1);
        assert!(lines
            .iter()
            .any(|l| l == "td token 0 names the recovery keyslot 0: never released"));
        assert!(lines
            .iter()
            .any(|l| l == "no transition this boot: td token 0 names the recovery keyslot 0"));
    }

    #[test]
    fn a_failed_step_stops_the_plan_and_the_next_boot_completes_it() {
        // The import fails: the volume opens with the first-boot secret,
        // and the next boot first removes the added keyslot as an orphan.
        let mut tpm = Scripted::new();
        let (mut disk, first) = installed(&mut tpm);
        disk.fail.push("import");
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, secret) = released(outcome, &lines);
        assert_eq!((keyslot, secret.expose()), (1, first.expose()));
        assert_eq!(
            disk.shape(),
            (vec![0, 1, 2], vec![(0, Some(1), Role::FirstBoot)])
        );
        disk.fail.clear();
        reboot(&tpm);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, new) = released(outcome, &lines);
        assert_eq!(keyslot, 2);
        assert_eq!(
            disk.calls,
            [
                "dump",
                "test 1",
                "kill 2",
                "add 2 by 1",
                "import 2",
                "test 2",
                "kill 1",
                "remove 0"
            ]
        );
        assert_eq!(disk.keyslots[&2], new.expose());
        assert_eq!(
            disk.shape(),
            (vec![0, 2], vec![(1, Some(2), Role::DeviceBound)])
        );

        // The kill fails after the new keyslot's test: the volume opens
        // with the new protector, and the next boot keeps it and retires
        // the leftover first-boot protector.
        let mut tpm = Scripted::new();
        let (mut disk, _first) = installed(&mut tpm);
        disk.fail.push("kill");
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, new) = released(outcome, &lines);
        assert_eq!(keyslot, 2);
        assert_eq!(disk.keyslots[&2], new.expose());
        disk.fail.clear();
        reboot(&tpm);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, secret) = released(outcome, &lines);
        assert_eq!((keyslot, secret.expose()), (2, new.expose()));
        assert_eq!(disk.calls, ["dump", "test 2", "kill 1", "remove 0"]);
        assert_eq!(
            disk.shape(),
            (vec![0, 2], vec![(1, Some(2), Role::DeviceBound)])
        );
    }

    #[test]
    fn without_a_tpm_device_nothing_is_capped_and_no_reseal_is_offered() {
        let tpm = Scripted::new();
        let mut disk = Disk::new(&tpm);
        let (outcome, lines) = boot_over::<Scripted>(&Ok(disk.header()), &mut disk, None);
        assert_eq!(recovered(outcome, &lines), (Recovery::NoTpm, false));
        assert!(disk.calls.is_empty());
        assert_eq!(tpm.pcr(12), [0; 32]);
        assert!(tpm.codes().is_empty());
    }

    #[test]
    fn a_refused_header_releases_nothing_and_is_still_capped() {
        let mut tpm = Scripted::new();
        let mut disk = Disk::new(&tpm);
        let error = "LUKS2 header copies disagree at the same sequence number 7";
        let (outcome, lines) = boot_over(&Err(error.into()), &mut disk, Some(&mut tpm));
        assert_eq!(
            recovered(outcome, &lines),
            (Recovery::Header(error.into()), false)
        );
        assert!(disk.calls.is_empty());
        assert_eq!(tpm.pcr(12), capped());
        assert_eq!(tpm.codes(), [PCR_READ, PCR_EXTEND, PCR_READ]);
    }

    /// Device-bound tokens are tried first, each role by number, and a
    /// token naming keyslot 0 never.
    #[test]
    fn device_bound_tokens_are_tried_first_and_keyslot_0_never() {
        let mut tpm = Scripted::new();
        let mut disk = Disk::new(&tpm);
        drop(disk.enroll(&mut tpm, 0, 1, Role::FirstBoot));
        drop(disk.enroll(&mut tpm, 1, 0, Role::DeviceBound));
        drop(disk.enroll(&mut tpm, 2, 3, Role::FirstBoot));
        drop(disk.enroll(&mut tpm, 3, 2, Role::DeviceBound));
        let header = disk.header();
        let order: Vec<u8> = candidates(&header).iter().map(|(n, _)| *n).collect();
        assert_eq!(order, [3, 0, 2]);
        assert_eq!(MAX_ATTEMPTS, 4);
    }

    /// The swtpm oracle over the release order: the first boot's seal,
    /// verification, cap and transition, the TPM's refusal after the cap,
    /// the next boot's release, a changed boot chain, and a cleared TPM.
    #[test]
    #[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
    fn emulator_release_runs_the_first_boot_transition_and_then_releases() {
        use crate::tests::emulator::{fresh_state, Emulator, Socket};
        struct Swtpm(Emulator);
        impl Tpm for Swtpm {
            type Transport = Socket;
            fn open(&mut self) -> Result<Client<Socket>, String> {
                Ok(self.0.client())
            }
        }
        fn measure(tpm: &Swtpm, image: u8) {
            tpm.0.client().extend_pcr(4, &[image; 32]).unwrap();
            tpm.0.client().extend_pcr(9, &[0x49; 32]).unwrap();
        }
        /// The disk model over the emulator: PCR 12 read through its own
        /// client before each cryptsetup command.
        fn disk_over(tpm: &Swtpm) -> Disk {
            let mut disk = Disk::new(&Scripted::new());
            let socket = tpm.0.socket();
            disk.pcr12 = Box::new(move || {
                crate::tests::emulator::client_at(&socket)
                    .read_pcr(CAP_PCR)
                    .unwrap()
            });
            disk.tpm_commands = None;
            disk
        }

        let mut tpm = Swtpm(Emulator::start(&fresh_state("release-oracle")));
        measure(&tpm, 0x44);
        let mut disk = disk_over(&tpm);
        let _first = disk.enroll(&mut tpm, 0, 1, Role::FirstBoot);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, new) = released(outcome, &lines);
        assert_eq!(keyslot, 2);
        assert_eq!(disk.keyslots[&2], new.expose());
        assert_eq!(
            disk.shape(),
            (vec![0, 2], vec![(1, Some(2), Role::DeviceBound)])
        );
        let Some(Entry::Td(bound)) = disk.tokens.get(&1).cloned() else {
            panic!("no device-bound token")
        };
        // After the cap the TPM refuses at PolicyPCR.
        match unseal_token(tpm.0.client(), &bound) {
            Err(error) => assert_eq!(
                error,
                UnsealError::PolicyRefused("TPM command 0x17f refused: 0x1c4".into())
            ),
            Ok(_) => panic!("released after the cap"),
        }

        // The next boot on the same chain releases the new protector.
        tpm.0.restart();
        measure(&tpm, 0x44);
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        let (keyslot, secret) = released(outcome, &lines);
        assert_eq!((keyslot, secret.expose()), (2, new.expose()));
        assert_eq!(disk.calls, ["test 2"]);

        // A changed selector image: the TPM refuses Unseal's policy.
        tpm.0.restart();
        measure(&tpm, 0x45);
        match unseal_token(tpm.0.client(), &bound) {
            Err(error) => assert_eq!(
                error,
                UnsealError::PolicyRefused("TPM command 0x15e refused: 0x99d".into())
            ),
            Ok(_) => panic!("released on another boot chain"),
        }
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        assert_eq!(
            recovered(outcome, &lines),
            (Recovery::NothingReleased, true)
        );
        drop(tpm);

        // A cleared TPM: a fresh state's storage primary loads nothing.
        let mut tpm = Swtpm(Emulator::start(&fresh_state("release-oracle-cleared")));
        measure(&tpm, 0x44);
        match unseal_token(tpm.0.client(), &bound) {
            Err(UnsealError::LoadRefused(error)) => {
                assert_eq!(error, "TPM command 0x157 refused: 0x1df")
            }
            Err(error) => panic!("not a load refusal: {error}"),
            Ok(_) => panic!("released on another TPM"),
        }
        let mut disk = disk_over(&tpm);
        disk.tokens.insert(1, Entry::Td(bound));
        disk.keyslots.insert(2, new.expose().to_vec());
        let (outcome, lines) = boot(&mut disk, &mut tpm);
        assert_eq!(
            recovered(outcome, &lines),
            (Recovery::NothingReleased, true)
        );
    }
}
