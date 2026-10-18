//! The selector's upgrade boot (td-install/ENCRYPTION.md "Re-encrypting
//! upgrade"; DESIGN.md "Upgrade boot"): before the cap the recovery key's
//! prompt, with U0's cancellation, each pending key or its drop, and in
//! the TPM shape the TPM's read, the new PIN and the verified seal; the
//! cap; then each state's plan, confirmed against cryptsetup's metadata,
//! the header read again after it, until the `td-upgrade` token is gone.
//! The mapping opens and the volume key is taken through `Host`; every TPM
//! operation goes through `Tpm`, every cryptsetup command through `Runner`.
//! A U5 header is the protected release's (`protected_release::
//! release_upgraded`), its `td-upgrade` token removed by `finish_u5`.

use crate::cryptsetup::{exit_code, KeyFile, Progress, Resilience, EXIT_BAD_PASSPHRASE};
use crate::luks2::{Kind, UpgradeView};
use crate::pin::{observe_pin, PinPcrs};
use crate::protected::{Shape, Token};
use crate::protected_plan;
use crate::protected_release::{
    entry, lockout_stage, new_pin, seal_new, unlock_description, Fido, FidoError, FidoToken,
    NewPin, Prompt,
};
use crate::recovery::{RecoveryKey, RecoveryText};
use crate::release::{Runner, Tpm};
use crate::upgrade::{self, Gathered, Input, Key, State, Step};
use crate::{cap, lockout, CapError, ObserveError, Secret};
use std::io;
use std::path::Path;

/// The recovery key's prompt in U0, whose empty entry asks to cancel.
pub const RECOVERY_PROMPT: &str = "td recovery key (empty to cancel protecting the disk): ";
/// The recovery key's prompt once nothing cancels.
pub const RECOVERY_AGAIN_PROMPT: &str = "td recovery key: ";
/// The cancellation's question, after an empty recovery-key entry in U0.
pub const CANCEL_PROMPT: &str =
    "Type cancel to cancel protecting the disk, anything else to go on: ";
/// The one answer that cancels.
pub const CANCEL_WORD: &[u8] = b"cancel";
/// The one answer at a pending key's prompt that drops it.
pub const DROP_WORD: &[u8] = b"drop";
pub const NO_MAINS: &str = "td: protecting the disk waits for mains power: this boot opens the \
     disk as before, and the next boot on mains power protects it";
pub const NO_CANCEL: &str = "td: protecting the disk has begun and can no longer be cancelled";
pub const ON_BATTERY: &str =
    "td: WARNING: the disk is being re-encrypted on battery power; keep the machine powered";
/// A boot that cannot finish a state's plan says so and halts: each
/// commit leaves a state the next boot continues from.
const STOPPED: &str = "the next boot continues protecting the disk; boot is refused";

/// The volume as the selector holds it.
pub trait Host {
    type VolumeKey: AsRef<[u8]>;
    /// The header as the partition holds it now.
    fn read(&mut self) -> Result<UpgradeView, String>;
    /// Opens the selector's mapping with `key`, on keyslot `slot` or, mid
    /// reencryption, naming none.
    fn open(&mut self, slot: Option<u8>, key: &[u8]) -> io::Result<()>;
    /// The open mapping's volume key, taken with `key`.
    fn volume_key(&mut self, key: &[u8]) -> io::Result<Self::VolumeKey>;
}

/// What the boot knows beside the header: the held partition's path, the
/// mapping's name, the power supply and the disk's resilience.
pub struct Boot<'a> {
    pub device: &'a Path,
    pub name: &'a str,
    pub on_mains: bool,
    pub resilience: Resilience,
}

/// What the upgrade boot decided.
pub enum Outcome<K> {
    /// The volume is protected, its mapping open: hand off `key` with an
    /// admission record of method 0.
    Upgraded { key: K },
    /// Cancelled in U0: the device-bound release order, then `cancel`.
    Cancel,
    /// U0 without mains power: the device-bound release order, nothing
    /// upgraded.
    NoMains,
    /// End of input, a failed applet, a cap that contradicts the seal, or
    /// a plan that cannot run: every secret is zeroed and boot is refused.
    Halt(String),
    /// The volume could not be read or opened.
    Failed(io::Error),
}

/// What the TPM shape's protector came to before the cap, and after a
/// take that found the primary changed.
enum Protector {
    None,
    Sealed(NewPin),
    Unusable,
    Changed,
}

/// The upgrade boot of an upgrading header in a state before U5.
#[allow(clippy::too_many_arguments)]
pub fn upgrade<T: Tpm, C: Runner, F: Fido, H: Host>(
    view: &UpgradeView,
    boot: &Boot<'_>,
    mut tpm: Option<&mut T>,
    cryptsetup: &mut C,
    fido: &mut F,
    host: &mut H,
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Outcome<H::VolumeKey> {
    let needs = match upgrade::needs(view) {
        Ok(needs) if needs.state != State::U5 => needs,
        Ok(_) => return Outcome::Halt("a U5 header releases as a protected one".into()),
        Err(refusal) => {
            let halt = format!("the upgrade cannot run: {refusal}; boot is refused");
            // Its removals lose nothing, so U0's cancellation stays
            // offered for a header `check` refuses.
            if upgrade::cancel(view).is_err() {
                return Outcome::Halt(halt);
            }
            console(&format!("td: the upgrade cannot run: {refusal}"));
            return match entry(prompt, CANCEL_PROMPT, "cancellation", console) {
                Ok(answer) if answer.as_bytes() == CANCEL_WORD => {
                    console("td: protecting the disk is cancelled");
                    Outcome::Cancel
                }
                Ok(_) => Outcome::Halt(halt),
                Err(reason) => Outcome::Halt(reason),
            };
        }
    };
    if needs.state.needs_mains() && !boot.on_mains {
        console(NO_MAINS);
        return Outcome::NoMains;
    }
    let mut recovery = match recovery_key(needs.state.cancellable(), prompt, console) {
        Ok(Some(key)) => key,
        Ok(None) => return Outcome::Cancel,
        Err(reason) => return Outcome::Halt(reason),
    };
    let mut fido2: Vec<(u8, Secret)> = Vec::new();
    let mut dropped: Vec<u8> = Vec::new();
    let count = needs.fido2.len();
    for (index, number) in needs.fido2.iter().enumerate() {
        match pending_key(
            view,
            *number,
            (index, count),
            &dropped,
            fido,
            prompt,
            console,
        ) {
            Ok(Some(secret)) => fido2.push((*number, secret)),
            Ok(None) => dropped.push(*number),
            Err(reason) => return Outcome::Halt(reason),
        }
    }
    let mut protector = Protector::None;
    if needs.shape == Shape::Tpm && (needs.tpm_pin || needs.lockout) {
        protector = match tpm.as_deref_mut() {
            None => unusable(console, "no TPM device"),
            Some(tpm) => match tpm_protector(view, needs.tpm_pin, tpm, prompt, console) {
                Ok(protector) => protector,
                Err(reason) => return Outcome::Halt(reason),
            },
        };
    }
    // The cap, whenever a TPM device exists.
    let verified = matches!(protector, Protector::Sealed(_));
    let capped = match tpm.as_deref_mut() {
        None => {
            console(
                "PCR 12 is not capped: no TPM device, so a TPM the kernel exposes later on \
                 this boot finds it open",
            );
            false
        }
        Some(tpm) => match tpm
            .open()
            .map_err(CapError::Uncertain)
            .and_then(|mut client| cap(&mut client))
        {
            Ok(()) => {
                console("PCR 12 release cap closed");
                true
            }
            Err(error @ (CapError::AlreadyClosed | CapError::NoSha256Bank)) if !verified => {
                console(&error.to_string());
                false
            }
            // A verified seal needed PCR 12 at zero on a SHA-256 bank.
            Err(error) => {
                return Outcome::Halt(format!(
                    "{error}; every gathered secret is zeroed and boot is refused"
                ))
            }
        },
    };
    // No take on a TPM this boot did not cap.
    if !capped && needs.shape == Shape::Tpm && !matches!(protector, Protector::Unusable) {
        protector = unusable(console, "this boot did not cap PCR 12");
    }
    let tpm = tpm.filter(|_| capped);
    let gathered = Secrets {
        fido2,
        dropped,
        protector,
    };
    after_cap(
        boot,
        gathered,
        &mut recovery,
        tpm,
        cryptsetup,
        host,
        prompt,
        console,
    )
}

/// What was gathered before the cap.
struct Secrets {
    fido2: Vec<(u8, Secret)>,
    dropped: Vec<u8>,
    protector: Protector,
}

impl Secrets {
    fn numbers(&self) -> Vec<u8> {
        self.fido2.iter().map(|(number, _)| *number).collect()
    }

    fn tpm(&self) -> upgrade::Tpm<'_> {
        match &self.protector {
            Protector::None => upgrade::Tpm::None,
            Protector::Sealed(new) => upgrade::Tpm::Sealed(&new.token),
            Protector::Unusable => upgrade::Tpm::Unusable,
            Protector::Changed => upgrade::Tpm::Changed,
        }
    }

    /// The bytes `key` names: the recovery key's digits, a pending key's
    /// passphrase or the new tpm-pin secret.
    fn of<'a>(&'a self, key: Key, recovery: &'a RecoveryText) -> io::Result<&'a [u8]> {
        let found = match key {
            Key::Recovery => Some(recovery.expose()),
            Key::Fido2 { token } => self
                .fido2
                .iter()
                .find(|(number, _)| *number == token)
                .map(|(_, secret)| secret.expose().as_slice()),
            Key::TpmPin => match &self.protector {
                Protector::Sealed(new) => Some(new.secret.expose().as_slice()),
                _ => None,
            },
        };
        found.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no key for the step"))
    }
}

/// How a state's plan ended.
enum Round {
    /// Every step committed: read the header again.
    Done,
    /// The recovery key did not open its keyslot: ask again, then plan the
    /// state again.
    WrongKey,
    Halt(String),
}

#[allow(clippy::too_many_arguments)]
fn after_cap<T: Tpm, C: Runner, H: Host>(
    boot: &Boot<'_>,
    mut gathered: Secrets,
    recovery: &mut RecoveryText,
    mut tpm: Option<&mut T>,
    cryptsetup: &mut C,
    host: &mut H,
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Outcome<H::VolumeKey> {
    let mut opened = false;
    let mut volume_key: Option<H::VolumeKey> = None;
    let mut rounds = 0;
    loop {
        if rounds == MAX_ROUNDS {
            return Outcome::Halt(format!("the upgrade did not end; {STOPPED}"));
        }
        let view = match host.read() {
            Ok(view) => view,
            Err(error) => return Outcome::Failed(io::Error::other(error)),
        };
        if view.header.kind() != Kind::Upgrading {
            return match (volume_key, opened) {
                (Some(key), true) => {
                    console("td: the disk is protected");
                    Outcome::Upgraded { key }
                }
                _ => Outcome::Halt(format!(
                    "the upgrade ended without the mapping and its key; {STOPPED}"
                )),
            };
        }
        let state = match upgrade::check(&view) {
            Ok((state, _)) => state,
            Err(refusal) => {
                return Outcome::Halt(format!("the upgrade stopped: {refusal}; {STOPPED}"))
            }
        };
        if !boot.on_mains && matches!(state, State::U1 | State::U2) {
            console(ON_BATTERY);
        }
        // A drop an earlier round removed is no longer the plan's, and its
        // number may hold the tpm-pin token now.
        let pending = upgrade::pending(&view.header);
        gathered.dropped.retain(|token| pending.contains(token));
        let numbers = gathered.numbers();
        let plan = upgrade::plan(
            &view,
            &Gathered {
                fido2: &numbers,
                dropped: &gathered.dropped,
                tpm: gathered.tpm(),
                resilience: boot.resilience,
            },
        );
        let steps = match plan
            .map_err(|refusal| refusal.to_string())
            .and_then(|plan| {
                let dump = cryptsetup
                    .metadata(boot.device)
                    .map_err(|error| error.to_string())?;
                plan.confirm(&dump).map_err(|refusal| refusal.to_string())
            }) {
            Ok(steps) => steps,
            Err(error) => return Outcome::Halt(format!("the upgrade stopped: {error}; {STOPPED}")),
        };
        let mut round = Run {
            boot,
            state,
            gathered: &mut gathered,
            recovery,
            opened: &mut opened,
            volume_key: &mut volume_key,
            cryptsetup,
            host,
            console,
        };
        let ended = round.run(&steps, tpm.as_deref_mut());
        match ended {
            Ok(Round::Done) => rounds += 1,
            Ok(Round::WrongKey) => match recovery_key(false, prompt, console) {
                Ok(Some(key)) => *recovery = key,
                Ok(None) => {}
                Err(reason) => return Outcome::Halt(reason),
            },
            Ok(Round::Halt(reason)) => return Outcome::Halt(reason),
            Err(error) => return Outcome::Failed(error),
        }
    }
}

/// The committed rounds a boot runs at most: each state's plan once, U4's
/// again after a take, far under it; a wrong recovery key spends none.
const MAX_ROUNDS: usize = 64;

/// One state's confirmed plan, run step by step.
struct Run<'a, 'c, C, H: Host> {
    boot: &'a Boot<'a>,
    state: State,
    gathered: &'a mut Secrets,
    recovery: &'a mut RecoveryText,
    opened: &'a mut bool,
    volume_key: &'a mut Option<H::VolumeKey>,
    cryptsetup: &'a mut C,
    host: &'a mut H,
    console: &'c mut dyn FnMut(&str),
}

impl<C: Runner, H: Host> Run<'_, '_, C, H> {
    fn run<T: Tpm>(&mut self, steps: &[Step], mut tpm: Option<&mut T>) -> io::Result<Round> {
        let count = steps.len();
        for (index, step) in steps.iter().enumerate() {
            let at = index.saturating_add(1);
            match step {
                Step::Reencrypt { slot, .. } if !*self.opened => {
                    if let Some(round) = self.open(Some(*slot))? {
                        return Ok(round);
                    }
                }
                Step::ResumeReencrypt if !*self.opened => {
                    if let Some(round) = self.open(None)? {
                        return Ok(round);
                    }
                }
                Step::TakeLockout => return Ok(self.take(tpm.as_deref_mut())),
                _ => {}
            }
            match self.step(step) {
                Ok(()) => (self.console)(&format!("upgrade step {at}/{count}: {}", describe(step))),
                Err(error) if is_recovery_test(step) && wrong_key(&error) => {
                    (self.console)(&format!(
                        "td: the recovery key does not open its keyslot: {error}"
                    ));
                    return Ok(Round::WrongKey);
                }
                Err(error) => {
                    return Ok(Round::Halt(format!(
                        "upgrade step {at}/{count} failed ({}): {error}; {STOPPED}",
                        describe(step)
                    )))
                }
            }
            // From U3 the recovery keyslot holds the new volume key: a boot
            // that starts there opens on it, and every boot takes the key
            // once, after that keyslot's test.
            if let Step::Test {
                slot,
                key: Key::Recovery,
            } = step
            {
                if matches!(self.state, State::U3 | State::U4) {
                    if !*self.opened {
                        if let Some(round) = self.open(Some(*slot))? {
                            return Ok(round);
                        }
                    }
                    if self.volume_key.is_none() {
                        *self.volume_key = Some(self.host.volume_key(self.recovery.expose())?);
                    }
                }
            }
        }
        Ok(Round::Done)
    }

    /// The mapping opened with the recovery key; a wrong key asks again.
    fn open(&mut self, slot: Option<u8>) -> io::Result<Option<Round>> {
        match self.host.open(slot, self.recovery.expose()) {
            Ok(()) => {
                *self.opened = true;
                (self.console)("td: the disk's mapping is open");
                Ok(None)
            }
            Err(error) if wrong_key(&error) => {
                (self.console)(&format!(
                    "td: the recovery key does not open the disk: {error}"
                ));
                Ok(Some(Round::WrongKey))
            }
            Err(error) => Err(error),
        }
    }

    fn step(&mut self, step: &Step) -> io::Result<()> {
        let json;
        let input: &[u8] = match step.input() {
            Input::Key(key) => self.gathered.of(key, self.recovery)?,
            Input::Token(token) => {
                json = token
                    .encode()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                json.as_bytes()
            }
            Input::Nothing => &[],
        };
        let file = match step.new_key() {
            Some(key) => Some(KeyFile::new(self.gathered.of(key, self.recovery)?)?),
            None => None,
        };
        let new_key = file.as_ref().map_or(Path::new(""), KeyFile::path);
        let Some(args) = step.args(self.boot.device, self.boot.name, new_key) else {
            return Err(io::Error::other("the take is no cryptsetup command"));
        };
        match step {
            Step::Reencrypt { .. } | Step::ResumeReencrypt => {
                let mut last = None;
                let console = &mut *self.console;
                self.cryptsetup
                    .reencrypt(&args, input, &mut |line| match line {
                        Progress::Bytes { done, size } if size > 0 => {
                            let percent = (done.saturating_mul(100) / size).min(100);
                            if last.is_none_or(|last| percent > last) {
                                console(&format!("td: re-encrypting the disk: {percent}%"));
                                last = Some(percent);
                            }
                        }
                        Progress::Bytes { .. } => {}
                        Progress::Unreadable => {
                            console("td: cryptsetup printed a progress line td cannot read")
                        }
                    })
            }
            _ => self.cryptsetup.run(&args, input),
        }
    }

    /// The lockout's take (ENCRYPTION.md "Lockout"), only while the storage
    /// primary's Name is the one the new tpm-pin token records ("Bus"). A
    /// changed primary plans U4 again with the protector removed; a take
    /// refused or failing before its token's import plans it as for an
    /// unusable TPM, and one failing after keeps its token, the header
    /// then reading U5.
    fn take<T: Tpm>(&mut self, tpm: Option<&mut T>) -> Round {
        let Some(tpm) = tpm else {
            self.gathered.protector = no_take(self.console, "no TPM this boot capped");
            return Round::Done;
        };
        // The header as this round's earlier steps left it: the take
        // follows the tpm-pin token's import.
        let view = match self.host.read() {
            Ok(view) => view,
            Err(error) => return Round::Halt(format!("the header: {error}; {STOPPED}")),
        };
        let recorded = view
            .header
            .protected
            .iter()
            .find_map(|(_, token)| match token {
                Token::TpmPin(pin) if pin.keyslot.is_some() => Some(pin.primary),
                _ => None,
            });
        let Some(recorded) = recorded else {
            self.gathered.protector = unusable(
                self.console,
                "no TPM PIN protector names a keyslot at the take",
            );
            return Round::Done;
        };
        // One read: the take runs under the Name it checked.
        let name = match tpm.primary_name() {
            Ok(name) if recorded.as_slice() == name.as_slice() => name,
            Ok(_) => {
                (self.console)(
                    "td: the TPM's storage primary is not the one the new TPM PIN protector \
                     was sealed under: it is removed, and the disk completes without it",
                );
                self.gathered.protector = Protector::Changed;
                return Round::Done;
            }
            Err(error) => {
                self.gathered.protector =
                    no_take(self.console, &format!("its storage primary: {error}"));
                return Round::Done;
            }
        };
        let Some(key) = self.volume_key.as_ref() else {
            return Round::Halt(format!("the take has no volume key; {STOPPED}"));
        };
        let view = &view;
        let import = |token: &crate::protected::Lockout| match upgrade::lockout_import(view, token)
        {
            Ok((number, Step::ImportToken { token, id, replace })) => Ok((
                number,
                protected_plan::Step::ImportToken { token, id, replace },
            )),
            Ok(_) => Err("the lockout import is no token import".to_string()),
            Err(refusal) => Err(refusal.to_string()),
        };
        let taken = lockout_stage(
            &view.header,
            self.boot.device,
            key.as_ref(),
            tpm,
            name,
            &mut *self.cryptsetup,
            &import,
            self.console,
        );
        if taken.is_some() {
            return Round::Done;
        }
        // An imported token keeps the protector: the header reads U5, and
        // the next protected release proves or takes it.
        let imported = match self.host.read() {
            Ok(view) => protected_plan::lockout(&view.header).is_some(),
            Err(error) => return Round::Halt(format!("the header: {error}; {STOPPED}")),
        };
        if imported {
            (self.console)(
                "td: the lockout token is imported but its take did not complete: the TPM PIN \
                 protector stays, and a later boot takes the lockout again",
            );
        } else {
            self.gathered.protector = no_take(self.console, "it did not complete");
        }
        Round::Done
    }
}

/// A take that did not reach its token's import: U4 then ends with the
/// tpm-pin protector imported before it and no lockout token.
fn no_take(console: &mut dyn FnMut(&str), why: &str) -> Protector {
    console(&format!(
        "td: no lockout take ({why}): the TPM PIN protector stays without a lockout token, \
         which a later boot takes"
    ));
    Protector::Unusable
}

fn unusable(console: &mut dyn FnMut(&str), why: &str) -> Protector {
    console(&format!(
        "td: the TPM is unusable for td ({why}): the disk completes as a TPM-primary volume \
         without the TPM PIN, whose boots ask for a security key or the recovery key"
    ));
    Protector::Unusable
}

fn is_recovery_test(step: &Step) -> bool {
    matches!(
        step,
        Step::Test {
            key: Key::Recovery,
            ..
        }
    )
}

fn wrong_key(error: &io::Error) -> bool {
    exit_code(error) == Some(EXIT_BAD_PASSPHRASE)
}

fn describe(step: &Step) -> String {
    match step {
        Step::Test { slot, .. } => format!("keyslot {slot} tested"),
        Step::KillSlot { slot, by } => {
            format!("keyslot {slot} destroyed, keyslot {by} authorizing")
        }
        Step::RemoveToken { token } => format!("token {token} removed"),
        Step::AddKeyslot { slot, by, .. } => {
            format!("keyslot {slot} added, keyslot {by} authorizing")
        }
        Step::ImportToken { token, id, replace } => {
            let (kind, role) = token.kind();
            let what = role.unwrap_or(kind);
            match (id, replace) {
                (Some(id), true) => format!("{what} token {id} replaced"),
                (Some(id), false) => format!("{what} token {id} imported"),
                (None, _) => format!("{what} token imported"),
            }
        }
        Step::Reencrypt { slot, .. } => format!("re-encrypted, keyslot {slot} authorizing"),
        Step::ResumeReencrypt => "re-encryption resumed and finished".into(),
        Step::TakeLockout => "lockout taken".into(),
    }
}

/// The recovery key, before anything else: in U0 an empty entry asks
/// whether to cancel, exactly `cancel` confirming (`Ok(None)`); after U0
/// an empty entry says nothing cancels. A key the codec refuses asks
/// again; end of input or a failed applet is the halt reason.
fn recovery_key(
    cancellable: bool,
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Result<Option<RecoveryText>, String> {
    let text = if cancellable {
        RECOVERY_PROMPT
    } else {
        RECOVERY_AGAIN_PROMPT
    };
    loop {
        let typed = entry(prompt, text, "recovery key", console)?;
        if typed.as_bytes().is_empty() {
            drop(typed);
            if !cancellable {
                console(NO_CANCEL);
                continue;
            }
            let answer = entry(prompt, CANCEL_PROMPT, "cancellation", console)?;
            if answer.as_bytes() == CANCEL_WORD {
                console("td: protecting the disk is cancelled");
                return Ok(None);
            }
            continue;
        }
        let parsed = RecoveryKey::parse(typed.as_bytes());
        drop(typed);
        match parsed {
            Ok(key) => return Ok(Some(key.passphrase())),
            Err(error) => console(&format!("{error}: enter it again")),
        }
    }
}

/// Pending token `number`'s key alone, at its own prompt: an empty entry
/// asks the connected key, exactly `drop` drops the token when
/// `upgrade::droppable` allows it (`Ok(None)`), and anything else, or a
/// key that releases nothing, asks again.
fn pending_key<F: Fido>(
    view: &UpgradeView,
    number: u8,
    (index, count): (usize, usize),
    dropped: &[u8],
    fido: &mut F,
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Result<Option<Secret>, String> {
    let header = &view.header;
    let Some(token) = header.protected.iter().find_map(|(at, token)| match token {
        Token::Fido2(fido) if *at == number && fido.keyslot.is_none() => Some(FidoToken {
            number,
            role: fido.role,
            credential: &fido.credential,
            salt: &fido.salt,
            x: &fido.x,
            y: &fido.y,
        }),
        _ => None,
    }) else {
        return Err(format!(
            "td token {number} is no pending security key; boot is refused"
        ));
    };
    let Some(volume) = lockout::volume_uuid(&header.uuid) else {
        return Err("the volume's UUID is not one a security key can be asked for".into());
    };
    let description = unlock_description(&header.uuid);
    let text = format!(
        "td new security key {} of {count} (token {number}): connect it and press Enter, or \
         type drop to leave it out: ",
        index.saturating_add(1)
    );
    let tokens = [token];
    loop {
        let typed = entry(prompt, &text, "new security key", console)?;
        if typed.as_bytes() == DROP_WORD {
            if upgrade::droppable(view, dropped, number) {
                console(&format!("td token {number} is dropped from the upgrade"));
                return Ok(None);
            }
            console(
                "td: this is the last fido2-primary key the disk is protected by: it cannot \
                 be dropped",
            );
            continue;
        }
        if !typed.as_bytes().is_empty() {
            console("td: press Enter with the key connected, or type drop");
            continue;
        }
        drop(typed);
        match fido.release(&tokens, &volume, &description, prompt, console) {
            Ok((0, secret)) => {
                console(&format!("td token {number}'s key answered"));
                return Ok(Some(secret));
            }
            Ok(_) => console("td: the security key answered for another token"),
            Err(FidoError::Retry(line)) => console(&line),
            Err(FidoError::Halt(reason)) => return Err(reason),
        }
    }
}

/// The TPM shape's read before the cap: unusable for td when its lockout
/// authorization is set with no lockout token to prove it or its state
/// cannot be read, or, when a protector is to be sealed, without measured
/// PCRs on a SHA-256 bank or when the seal or its verifying Unseal fails;
/// otherwise the new PIN twice and the verified seal. `Err` is the halt
/// reason, which only the PIN prompts give.
fn tpm_protector<T: Tpm>(
    view: &UpgradeView,
    seal: bool,
    tpm: &mut T,
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Result<Protector, String> {
    match tpm.dictionary_attack() {
        Ok(state) if state.lockout_auth_set && protected_plan::lockout(&view.header).is_none() => {
            return Ok(unusable(
                console,
                "another system set its lockout authorization, which td cannot prove",
            ))
        }
        Ok(_) => {}
        Err(error) => return Ok(unusable(console, &format!("its lockout state: {error}"))),
    }
    if !seal {
        return Ok(Protector::None);
    }
    // 8c names no PCR 7: whether Secure Boot is on is 8i's reading.
    let policy = match tpm
        .open()
        .map_err(ObserveError::Tpm)
        .and_then(|mut client| observe_pin(&mut client, PinPcrs::Selector))
    {
        Ok(policy) => policy,
        Err(error) => return Ok(unusable(console, &error.to_string())),
    };
    let pin = new_pin(prompt, console)?;
    Ok(
        match seal_new(policy, None, &pin, tpm, "no TPM PIN protector", console) {
            Some(new) => Protector::Sealed(new),
            None => unusable(console, "its seal did not verify"),
        },
    )
}

/// U0's cancellation after the device-bound release order opened the
/// volume: the `td-upgrade` token's removal, then the pending tokens',
/// confirmed against cryptsetup's metadata; a failed removal stops it.
pub fn cancel<C: Runner>(
    view: &UpgradeView,
    device: &Path,
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) {
    let steps = match upgrade::cancel(view)
        .map_err(|refusal| refusal.to_string())
        .and_then(|plan| {
            let dump = cryptsetup
                .metadata(device)
                .map_err(|error| error.to_string())?;
            plan.confirm(&dump).map_err(|refusal| refusal.to_string())
        }) {
        Ok(steps) => steps,
        Err(error) => {
            console(&format!(
                "the cancellation writes nothing this boot: {error}"
            ));
            return;
        }
    };
    removals(&steps, device, cryptsetup, "cancellation", console);
}

/// A U5 boot's plan after the protected release opened the volume: the
/// `td-upgrade` token's removal, confirmed against cryptsetup's metadata.
/// `false` when it did not run: the boot then runs no protected plan, and
/// the next boot finds U5 again.
pub fn finish_u5<C: Runner>(
    view: &UpgradeView,
    device: &Path,
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) -> bool {
    let empty = Gathered {
        fido2: &[],
        dropped: &[],
        // U5's plan reencrypts nothing.
        tpm: upgrade::Tpm::None,
        resilience: Resilience::Journal,
    };
    let steps = match upgrade::check(view)
        .map_err(|refusal| refusal.to_string())
        .and_then(|(state, _)| {
            if state != State::U5 {
                return Err("the header is not in U5".to_string());
            }
            let plan = upgrade::plan(view, &empty).map_err(|refusal| refusal.to_string())?;
            let dump = cryptsetup
                .metadata(device)
                .map_err(|error| error.to_string())?;
            plan.confirm(&dump).map_err(|refusal| refusal.to_string())
        }) {
        Ok(steps) => steps,
        Err(error) => {
            console(&format!(
                "the upgrade's end writes nothing this boot: {error}"
            ));
            return false;
        }
    };
    removals(&steps, device, cryptsetup, "upgrade", console)
}

/// Token removals, the only steps a cancellation or a U5 plan holds, each
/// reported; the first failure stops them. `true` once all committed.
fn removals<C: Runner>(
    steps: &[Step],
    device: &Path,
    cryptsetup: &mut C,
    what: &str,
    console: &mut dyn FnMut(&str),
) -> bool {
    let count = steps.len();
    for (index, step) in steps.iter().enumerate() {
        let at = index.saturating_add(1);
        let ran = match step {
            Step::RemoveToken { .. } => match step.args(device, "", Path::new("")) {
                Some(args) => cryptsetup.run(&args, &[]),
                None => Err(io::Error::other("no command")),
            },
            _ => Err(io::Error::other("not a token removal")),
        };
        match ran {
            Ok(()) => console(&format!("{what} step {at}/{count}: {}", describe(step))),
            Err(error) => {
                console(&format!(
                    "{what} step {at}/{count} failed ({}): {error}",
                    describe(step)
                ));
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
#[path = "upgrade_boot_tests.rs"]
mod tests;
