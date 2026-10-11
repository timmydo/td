//! The protected tier's release (td-install/ENCRYPTION.md "Protected
//! release", "PIN and dictionary-attack policy" and its "Lockout";
//! DESIGN.md "Protected roles", "Release"): before the cap the chain
//! check and its warning, the TPM PIN, a security key through `Fido`, the
//! recovery key and the verified reseal; the cap; the released keyslot's
//! test; then, once the selector holds the volume key (`finish`), the plan,
//! the lockout's take or proof, the counter and its reset. A selector
//! update's pre-seal (ENCRYPTION.md "Selector update") seals the PIN's
//! released secret to the caller's predicted PCRs before the cap, and
//! `write_tokens` runs its commit's token steps. Every TPM
//! operation opens its own client through `Tpm`, every cryptsetup command
//! runs through `Runner`, and every secret is zeroed when dropped. Library
//! code: td-boot dispatches a protected header here.

use crate::cryptsetup::{exit_code, test_args, KeyFile, EXIT_BAD_PASSPHRASE};
use crate::luks2::{Header, Kind, MAX_FIDO2_TOKENS};
use crate::pin::{
    chain_check, observe_pin, protected_auth_policy, release_pin_policy, ChainError, Pin, PinPcrs,
    PinPolicy, PRIMARY_NAME_LEN, SALT_LEN,
};
use crate::protected::{
    FidoRole, Shape, Token, TpmPin, MAX_PIN_PRIVATE_BYTES, MAX_PIN_PUBLIC_BYTES,
};
use crate::protected_plan::{self, Input, Opened, Step};
use crate::recovery::{RecoveryKey, RecoveryText};
use crate::release::{LockoutCommand, Runner, Tpm};
use crate::{cap, lockout, read_random, CapError, ObserveError, Secret, UnsealError};
use crate::{POLICY_PCR_REFUSED, SECRET_LEN};
use std::io;
use std::path::Path;
use td_tpm::{AuthRefusal, Client, LockoutAuth, LockoutError, SealedObject, Transport};

/// The TPM PIN's prompt (ENCRYPTION.md "Protected release").
pub const PIN_PROMPT: &str = "td PIN (empty for another way): ";
/// The prompt for a security key or the recovery key.
pub const ANOTHER_WAY_PROMPT: &str =
    "td: connect one security key and press Enter, or type the recovery key: ";
/// A reseal's new PIN, asked twice.
pub const NEW_PIN_PROMPT: &str = "td new PIN: ";
pub const NEW_PIN_AGAIN_PROMPT: &str = "td new PIN again: ";
/// The reseal question, asked through secret-line so that nothing typed
/// before it can answer it.
pub const RESEAL_PROMPT: &str =
    "Type reseal to seal a new TPM PIN protector to this boot chain, anything else to boot once: ";
/// The one answer that confirms the reseal.
pub const RESEAL_WORD: &[u8] = b"reseal";
/// The warning when the TPM loads a tpm-pin object whose policy this boot
/// chain does not meet, printed before any other prompt.
pub const CHAIN_WARNING: &str = "td: WARNING: this machine's boot chain is not the one td sealed. \
     Firmware, its settings or option ROMs, Secure Boot, or td's selector changed. If you did \
     not change them, someone may have: a security key or the recovery key used now unlocks \
     the disk for whatever is running.";
/// The warning when the TPM does not load the object, or its storage
/// primary is not the one the token recorded.
pub const TPM_WARNING: &str = "td: WARNING: this machine's TPM was cleared or replaced, or \
     something is intercepting it. If you did not clear or replace it, someone may have: a \
     security key or the recovery key used now unlocks the disk for whatever is running.";
pub const NO_TPM: &str =
    "td: no TPM device: the TPM PIN cannot release; use a security key or the recovery key";
pub const NO_SHA256_BANK: &str = "td: the TPM has no SHA-256 PCR bank: the TPM PIN cannot \
     release; use a security key or the recovery key";
pub const LOCKED_OUT: &str = "td: the TPM is locked out after too many wrong PINs and takes \
     one more every 10 minutes it is powered; use a security key or the recovery key";
pub const NO_PIN_PROTECTOR: &str =
    "td: this volume holds no TPM PIN protector; use a security key or the recovery key";
/// After a security-key or recovery-key release on a header with no lockout
/// token: this boot's take writes one, which anchors the next boot's reseal.
pub const NO_ANCHOR: &str = "no reseal is offered: this volume holds no lockout token to anchor \
     a new TPM PIN protector; this boot takes the TPM's lockout, and the next boot by a security \
     key or the recovery key offers the reseal";
/// Printed before the first PIN prompt: the consoles read key positions
/// under the kernel's US keymap ("PIN and dictionary-attack policy").
pub const PIN_KEYS: &str = "the TPM PIN is read at its US key positions whatever the keys \
     show; a PIN of digits alone avoids the question; nothing typed is shown";
/// Printed before the first prompt for another way.
pub const ANOTHER_WAY_KEYS: &str = "an empty entry asks for a security key; the recovery key \
     is its 48 digits on the keyboard's top row, unshifted, spaces or hyphens between groups \
     optional, keypad digits only with Num Lock on; nothing typed is shown";
/// What confirming a reseal does, printed before the question.
pub const RESEAL_WARNING: &str = "a reseal binds release to this boot chain under a new TPM PIN \
     and retires every other TPM PIN protector; security keys and the recovery key stay";
/// Printed before the question: the answer and the new PIN are read at
/// their US key positions.
pub const RESEAL_KEYS: &str =
    "type the answer and the new PIN at their US key positions: the keyboard is read as a US one";

/// One console entry, zeroed on drop and neither `Debug`, `Display` nor
/// `Clone`.
pub struct Entry(Box<[u8]>);
impl Drop for Entry {
    fn drop(&mut self) {
        td_tpm::zero(&mut self.0);
    }
}
impl Entry {
    /// A copy of `bytes`; the caller zeroes its own.
    pub fn new(bytes: &[u8]) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// How one run of secret-line ended.
pub enum Entered {
    /// A whole line, without its newline.
    Line(Entry),
    /// End of input before a newline.
    Eof,
    /// A record secret-line refused whole: over-long, or holding a newline.
    Refused,
}

/// The console's input: one run of td-init's secret-line per entry, which
/// prints the prompt with echo off. An error is an applet that failed,
/// which halts as end of input does.
pub trait Prompt {
    fn read(&mut self, prompt: &str) -> Result<Entered, String>;
}

/// One header fido2 token naming a keyslot, as the security-key release
/// takes it.
pub struct FidoToken<'a> {
    pub number: u8,
    pub role: FidoRole,
    pub credential: &'a [u8],
    pub salt: &'a [u8; SALT_LEN],
    pub x: &'a [u8; 32],
    pub y: &'a [u8; 32],
}

/// Why a security key released nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FidoError {
    /// A console line, then the prompt again: none or several keys, a key
    /// enrolled under none of the tokens, a PIN status, a failed or
    /// unverified assertion.
    Retry(String),
    /// End of input or a failed secret-line at the key's PIN prompt.
    Halt(String),
}

/// The selector's FIDO2 client (ENCRYPTION.md "FIDO2 protectors"), which
/// td-boot's worker implements and tests script: one key's silent identify
/// among `tokens`, its PIN through `prompt`, the assertion verified under
/// the token's public key, and the keyslot passphrase derived for
/// `volume`, the index into `tokens` naming the token. `description` is
/// the operation's canonical description (`unlock_description`).
pub trait Fido {
    fn release(
        &mut self,
        tokens: &[FidoToken<'_>],
        volume: &[u8; 16],
        description: &[u8],
        prompt: &mut dyn Prompt,
        console: &mut dyn FnMut(&str),
    ) -> Result<(usize, Secret), FidoError>;
}

/// The canonical description an unlock's assertions carry in their
/// client-data hash: the volume they open.
pub fn unlock_description(uuid: &str) -> Vec<u8> {
    format!("td disk unlock volume {uuid}").into_bytes()
}

/// Which protector released, which the admission record follows
/// (ENCRYPTION.md "Verified account handoff").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    TpmPin,
    Fido2Primary,
    Fido2Recovery,
    RecoveryKey,
}
impl Method {
    /// A primary protector's release admits one session; a recovery one
    /// unlocks storage alone.
    pub fn admits(self) -> bool {
        matches!(self, Self::TpmPin | Self::Fido2Primary)
    }
}

/// The key whose keyslot's test passed: a protector secret or a security
/// key's passphrase, or the recovery key's 48 digits.
pub enum Key {
    Secret(Secret),
    Recovery(RecoveryText),
}
impl Key {
    /// The bytes cryptsetup takes on standard input.
    pub fn expose(&self) -> &[u8] {
        match self {
            Self::Secret(secret) => secret.expose(),
            Self::Recovery(text) => text.expose(),
        }
    }
}

/// A reseal's new tpm-pin protector, verified before the cap.
struct NewPin {
    token: TpmPin,
    secret: Secret,
}

/// A release whose keyslot's test passed. The selector opens the mapping
/// and takes the volume key with `key`, then hands this to `finish`.
pub struct Released {
    pub keyslot: u8,
    pub key: Key,
    pub method: Method,
    opened: Opened,
    reseal: Option<NewPin>,
    /// A selector update's pre-seal: the PIN's released secret sealed under
    /// the typed PIN to the predicted chain, naming no keyslot yet.
    predicted: Option<Box<TpmPin>>,
    /// This boot's own cap closed PCR 12, on a TPM with a SHA-256 bank.
    capped: bool,
}

/// A staged selector update's predicted PCRs (ENCRYPTION.md "Selector
/// update", "Prediction"): PCR 7 as it reads, PCRs 4 and 9 replayed with
/// the staged pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Predicted {
    pub pcr4: [u8; 32],
    pub pcr7: [u8; 32],
    pub pcr9: [u8; 32],
}

impl Predicted {
    /// The values a policy over `pcrs` takes, in its order.
    fn values(&self, pcrs: PinPcrs) -> Option<Vec<[u8; 32]>> {
        pcrs.measured()
            .iter()
            .map(|pcr| match pcr {
                4 => Some(self.pcr4),
                7 => Some(self.pcr7),
                9 => Some(self.pcr9),
                _ => None,
            })
            .collect()
    }
}

/// The selector's pre-seal hook: after a TPM PIN release, before the cap,
/// the predicted PCRs of a staged update it read and predicted, or `None`
/// when there is none or the prediction failed, which it has said.
pub type PreSeal<'a> = &'a mut dyn FnMut(&mut dyn FnMut(&str)) -> Option<Predicted>;

/// What the protected release decided.
pub enum Outcome {
    Released(Released),
    /// End of input or a failed secret-line, or a cap that is uncertain,
    /// mismatched or contradicts a release: every secret is zeroed and the
    /// selector refuses boot and halts.
    Halt(String),
    /// A keyslot test that failed other than as a wrong key: the boot fails
    /// as a failed open does.
    Failed(io::Error),
}

/// What a tpm-pin chain check found (ENCRYPTION.md "Protected release").
#[derive(Default)]
struct Chain {
    /// The token whose object can release here.
    passed: Option<(u8, TpmPin)>,
    /// The storage primary's Name every tpm-pin token records, which a
    /// reseal requires again; `None` with no tpm-pin token on the header,
    /// when the reseal is a first seal.
    recorded: Option<[u8; PRIMARY_NAME_LEN]>,
    /// The tokens record different Names, or the TPM's primary is not the
    /// recorded one: no reseal is offered.
    other_primary: bool,
}

/// The protected release order over `header`, a protected header td's
/// reader returned (step 1, td-boot's). `device` names the partition to
/// cryptsetup; `tpm` is `None` when no TPM device appeared within
/// td-boot's wait. Before the cap: the chain check in the TPM-primary
/// shape, its warning before any prompt, the TPM PIN while an object
/// passes and the TPM is not locked out, then a security key or the
/// recovery key, and after either, in the TPM-primary shape with no object
/// passing, the offered reseal, sealed and verified before the cap. The
/// cap whenever a TPM device exists. After it, the released keyslot's
/// test, a wrong key asking again for a security key or the recovery key.
/// After a TPM PIN release `preseal` runs, still before the cap, and a
/// prediction it returns is sealed under the typed PIN. No cryptsetup
/// command runs before the cap, and no secret but the one returned and a
/// verified reseal's outlives this.
#[allow(clippy::too_many_arguments)]
pub fn release<T: Tpm, C: Runner, F: Fido>(
    header: &Header,
    device: &Path,
    mut tpm: Option<&mut T>,
    cryptsetup: &mut C,
    fido: &mut F,
    prompt: &mut dyn Prompt,
    preseal: PreSeal<'_>,
    console: &mut dyn FnMut(&str),
) -> Outcome {
    // td-boot dispatches only a protected header here; an upgrading one
    // boots as device-bound until 8f.
    if header.kind() != Kind::Protected {
        return Outcome::Halt("the header is not a protected one; boot is refused".into());
    }
    let shape = protected_plan::shape(header);
    let mut ways = Ways {
        header,
        fido,
        hinted: false,
    };
    let mut chain = Chain::default();
    let mut released = None;
    let mut predicted = None;
    match tpm.as_deref_mut() {
        None if shape == Shape::Tpm => console(NO_TPM),
        None => {}
        Some(tpm) if shape == Shape::Tpm => {
            chain = chain_stage(header, tpm, console);
            if let Some((number, token)) = &chain.passed {
                match pin_loop(*number, token, tpm, prompt, console) {
                    Ok(Some((secret, pin))) => {
                        predicted = preseal(console)
                            .and_then(|values| {
                                seal_predicted(token, &values, &pin, &secret, tpm, console)
                            })
                            .map(Box::new);
                        drop(pin);
                        released = Some((Opened::TpmPin { token: *number }, Key::Secret(secret)))
                    }
                    Ok(None) => {}
                    Err(reason) => return Outcome::Halt(reason),
                }
            }
        }
        Some(_) => {}
    }
    let (opened, key) = match released {
        Some(released) => released,
        None => match another_way(&mut ways, prompt, console) {
            Ok(released) => released,
            Err(reason) => return Outcome::Halt(reason),
        },
    };
    let mut reseal = match tpm.as_deref_mut() {
        Some(tpm)
            if shape == Shape::Tpm
                && chain.passed.is_none()
                && !matches!(opened, Opened::TpmPin { .. }) =>
        {
            match offer_reseal(header, opened, &chain, tpm, prompt, console) {
                Ok(reseal) => reseal,
                Err(reason) => return Outcome::Halt(reason),
            }
        }
        _ => None,
    };
    // The cap, whenever a TPM device exists, whatever came before.
    let verified = matches!(opened, Opened::TpmPin { .. }) || reseal.is_some();
    let capped = match tpm {
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
            // A TPM PIN release or a verified reseal needed PCR 12 at zero
            // on a SHA-256 bank: a cap that says otherwise contradicts it.
            Err(error) => {
                return Outcome::Halt(format!(
                    "{error}; every released secret is zeroed and boot is refused"
                ))
            }
        },
    };
    // After the cap: the released keyslot's test.
    let (mut opened, mut key) = (opened, key);
    let keyslot = loop {
        let Some(slot) = keyslot_of(header, opened) else {
            return Outcome::Halt("the released protector names no keyslot".into());
        };
        match cryptsetup.run(&test_args(device, slot), key.expose()) {
            Ok(()) => {
                console(&format!("keyslot {slot} opens"));
                break slot;
            }
            Err(error) if exit_code(&error) == Some(EXIT_BAD_PASSPHRASE) => {
                console(&format!(
                    "keyslot {slot} does not open with what {} released: {error}",
                    describe(header, opened)
                ));
                drop(key);
                if predicted.take().is_some() {
                    console(
                        "the update's predicted protector seals what does not open: it is \
                         discarded",
                    );
                }
                // A reseal confirmed after a key that opens nothing is
                // someone's, not the owner's: the next key's release must
                // not install it.
                if reseal.take().is_some() {
                    console(
                        "the reseal confirmed after that key is discarded: no keyslot is \
                         added this boot",
                    );
                }
                match another_way(&mut ways, prompt, console) {
                    Ok((next, next_key)) => {
                        opened = next;
                        key = next_key;
                    }
                    Err(reason) => return Outcome::Halt(reason),
                }
            }
            Err(error) => return Outcome::Failed(error),
        }
    };
    Outcome::Released(Released {
        keyslot,
        key,
        method: method(header, opened),
        opened,
        reseal,
        predicted,
        capped,
    })
}

/// What `finish` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Finished {
    /// The resealed protector's keyslot, once its test passed.
    pub resealed: Option<u8>,
    /// This boot took or proved the TPM's lockout authorization.
    pub lockout: bool,
    /// A selector update's predicted protector, naming the keyslot that
    /// released, for the commit to import; none once this boot refused
    /// any header change, or without a lockout this boot took or proved
    /// whose token anchors the object's primary.
    pub predicted: Option<TpmPin>,
}

/// After the selector opened the mapping and took `volume_key` with the
/// released key: one confirmation of cryptsetup's metadata before the
/// first write; without a reseal the plan, then in the TPM-primary shape
/// the lockout's take or proof; with one, the take or proof first and the
/// reseal's plan only once it succeeded, so a take or proof that fails
/// discards the object before its import; then the counter, and its reset
/// after a security-key or recovery-key release. The TPM is reached only
/// when this boot's own cap closed PCR 12; nothing here halts the boot.
pub fn finish<T: Tpm, C: Runner>(
    released: Released,
    header: &Header,
    device: &Path,
    volume_key: &[u8],
    tpm: Option<&mut T>,
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) -> Finished {
    let Released {
        keyslot: opener,
        key,
        method,
        opened,
        reseal,
        predicted,
        capped,
    } = released;
    let mut finished = Finished::default();
    let dropped = |console: &mut dyn FnMut(&str)| {
        if predicted.is_some() {
            console("the update's predicted protector is discarded: the header is unchanged");
        }
    };
    let mut tpm = tpm.filter(|_| capped);
    let discard = |console: &mut dyn FnMut(&str), why: &str| {
        console(&format!(
            "the resealed protector is discarded before its import: {why}"
        ))
    };
    let shape = match protected_plan::check(header) {
        Ok(shape) => shape,
        Err(refusal) => {
            console(&format!("no header change this boot: {refusal}"));
            if reseal.is_some() {
                discard(console, &refusal.to_string());
            }
            dropped(console);
            return finished;
        }
    };
    let plan = match protected_plan::plan(header, opened, reseal.as_ref().map(|new| &new.token)) {
        Ok(plan) => Some(plan),
        Err(refusal) => {
            console(&format!("no plan this boot: {refusal}"));
            None
        }
    };
    let lockout_due = shape == Shape::Tpm && tpm.is_some();
    if plan.as_ref().is_some_and(|plan| !plan.is_empty()) || lockout_due {
        let confirmed = cryptsetup
            .metadata(device)
            .map_err(|error| error.to_string())
            .and_then(|dump| {
                protected_plan::confirm(&header.metadata(), &dump)
                    .map_err(|refusal| refusal.to_string())
            });
        if let Err(error) = confirmed {
            console(&format!("no header change this boot: {error}"));
            if reseal.is_some() {
                discard(console, "cryptsetup's metadata does not confirm the header");
            }
            dropped(console);
            return finished;
        }
    }
    let keys = Keys {
        opener,
        opener_key: key.expose(),
        new: None,
    };
    if reseal.is_none() {
        if let Some(plan) = &plan {
            run_steps(plan.steps(), device, &keys, cryptsetup, console);
        }
    }
    // The counter as the release left it, before a take's reset.
    let counter = match tpm.as_deref_mut() {
        Some(tpm) if shape == Shape::Tpm => Some(tpm.dictionary_attack()),
        _ => None,
    };
    let lockout = match tpm.as_deref_mut() {
        Some(tpm) if lockout_due => {
            lockout_stage(header, device, volume_key, tpm, cryptsetup, console)
        }
        _ => None,
    };
    if let Some(new) = &reseal {
        // The seal's primary must be the one a lockout token already on
        // this header records under the volume key: the header's tpm-pin
        // tokens, and so the Name a reseal required before the cap, are
        // unauthenticated, and a first seal required none.
        let anchored = protected_plan::anchors(header, volume_key, &new.token.primary);
        match (&lockout, &plan) {
            (Some(_), Some(_)) if !anchored => discard(
                console,
                "no lockout token on this header verifies under the volume key for the \
                 storage primary it was sealed under",
            ),
            (Some(_), Some(plan)) => {
                let new_slot = plan.steps().iter().find_map(|step| match step {
                    Step::AddKeyslot { slot, .. } => Some(*slot),
                    _ => None,
                });
                let keys = Keys {
                    new: new_slot.map(|slot| (slot, &new.secret)),
                    ..keys
                };
                finished.resealed = run_steps(plan.steps(), device, &keys, cryptsetup, console);
                if let Some(slot) = finished.resealed {
                    console(&format!(
                        "reseal: keyslot {slot} holds a TPM PIN protector sealed to this boot \
                         chain, verified before the cap"
                    ));
                }
            }
            (None, _) => discard(
                console,
                "td holds no proven lockout authorization for this TPM",
            ),
            (Some(_), None) => discard(console, "the planner refused it"),
        }
    }
    drop(reseal);
    // The update's predicted protector goes on to the commit under the
    // reseal's rule: only beside a lockout this boot took or proved, whose
    // token on the header read at release records the object's primary
    // under the volume key.
    if let Some(token) = predicted {
        let why = if lockout.is_none() {
            Some("td holds no proven lockout authorization for this TPM")
        } else if !protected_plan::anchors(header, volume_key, &token.primary) {
            Some(
                "no lockout token on this header verifies under the volume key for the storage \
                 primary it was sealed under",
            )
        } else {
            None
        };
        match why {
            Some(why) => console(&format!(
                "the update's predicted protector is discarded: {why}"
            )),
            None => {
                finished.predicted = Some(TpmPin {
                    keyslot: Some(opener),
                    ..*token
                });
            }
        }
    }
    match counter {
        Some(Ok(state)) if state.counter > 0 => console(&format!(
            "td: the TPM counted {} wrong PINs since its last reset",
            state.counter
        )),
        Some(Err(error)) => console(&format!("the TPM's counter cannot be read: {error}")),
        _ => {}
    }
    if let (Some((auth, primary)), Some(tpm)) = (&lockout, tpm) {
        finished.lockout = true;
        if method != Method::TpmPin {
            match tpm.lockout(LockoutCommand::Reset, primary, auth) {
                Ok(()) => console("td: the TPM's wrong-PIN counter is reset"),
                Err(error) => console(&format!("the TPM's counter reset failed: {error}")),
            }
        }
    }
    finished
}

/// The deployment initramfs's post-cap check of a tpm-pin token
/// (ENCRYPTION.md "Protected release"): a policy session's PolicyPCR over
/// the token's PCRs as they read now and a literal-zero PCR 12, which the
/// selector's closed cap refuses with `TPM_RC_VALUE` before any
/// authorization. It loads no object and sends no Unseal or PIN. `Ok` is a
/// PolicyPCR the TPM accepted, release still open, on which the caller
/// halts as on a release; `PolicyRefused` and `NoSha256Bank` release
/// nothing.
pub fn policy_pcr_alone<T: Transport>(
    mut client: Client<T>,
    token: &TpmPin,
) -> Result<(), UnsealError> {
    let policy = release_pin_policy(&mut client, token.pcrs)?;
    match client.policy_session(&policy.pcr_policy(), false) {
        Ok(_) => Ok(()),
        Err(error) => match client.last_refusal() {
            Some(refusal) if refusal == POLICY_PCR_REFUSED => {
                Err(UnsealError::PolicyRefused(error))
            }
            _ => Err(UnsealError::Other(error)),
        },
    }
}

/// The keyslot the protector `opened` names: its token's, or the marker's
/// for the recovery key.
fn keyslot_of(header: &Header, opened: Opened) -> Option<u8> {
    let token = |number: u8| {
        header
            .protected
            .iter()
            .find(|(n, _)| *n == number)
            .and_then(|(_, token)| token.keyslot())
    };
    match opened {
        Opened::TpmPin { token: number } | Opened::Fido2 { token: number } => token(number),
        Opened::RecoveryKey => protected_plan::marker(header).map(|(_, slot)| slot),
    }
}

fn method(header: &Header, opened: Opened) -> Method {
    match opened {
        Opened::TpmPin { .. } => Method::TpmPin,
        Opened::RecoveryKey => Method::RecoveryKey,
        Opened::Fido2 { token } => {
            let primary = header.protected.iter().any(|(number, t)| {
                *number == token
                    && matches!(t, Token::Fido2(fido) if fido.role == FidoRole::Primary)
            });
            if primary {
                Method::Fido2Primary
            } else {
                Method::Fido2Recovery
            }
        }
    }
}

fn describe(header: &Header, opened: Opened) -> String {
    match opened {
        Opened::TpmPin { token } => format!("td token {token} (tpm-pin)"),
        Opened::Fido2 { token } => {
            let role = match method(header, opened) {
                Method::Fido2Primary => FidoRole::Primary,
                _ => FidoRole::Recovery,
            };
            format!("td token {token} ({})", role.name())
        }
        Opened::RecoveryKey => "the recovery key".into(),
    }
}

/// The chain check of every tpm-pin token naming a keyslot, by number,
/// until one passes; when none does, the warning, before any prompt.
fn chain_stage<T: Tpm>(header: &Header, tpm: &mut T, console: &mut dyn FnMut(&str)) -> Chain {
    let mut chain = Chain::default();
    let all = protected_plan::tpm_pins(header);
    for (_, token) in &all {
        match chain.recorded {
            Some(name) if name != token.primary => chain.other_primary = true,
            _ => chain.recorded = Some(token.primary),
        }
    }
    let pins: Vec<(u8, &TpmPin)> = all
        .into_iter()
        .filter(|(_, token)| token.keyslot.is_some())
        .collect();
    if pins.is_empty() {
        // The TPM-primary shape always holds one: its absence is a cut
        // update, a cleared TPM's re-enrollment not yet done, or a header
        // someone rewrote, and a key used now may be the one they await.
        console(NO_PIN_PROTECTOR);
        console(CHAIN_WARNING);
        return chain;
    }
    let (mut changed, mut replaced, mut no_bank) = (false, false, false);
    for (number, token) in pins {
        let checked = tpm
            .open()
            .map_err(ChainError::Other)
            .and_then(|client| chain_check(client, token.pcrs, &token.primary, &token.sealed));
        match checked {
            Ok(()) => {
                chain.passed = Some((number, token.clone()));
                return chain;
            }
            Err(error) => {
                console(&format!("td token {number} (tpm-pin): {error}"));
                match error {
                    ChainError::ChangedChain => changed = true,
                    // A public area that is not td's tpm-pin object.
                    ChainError::NotProtected(_) => changed = true,
                    ChainError::NoSha256Bank => no_bank = true,
                    // The storage primary matched the recorded Name; the
                    // private area did not load under it.
                    ChainError::LoadRefused(_) => replaced = true,
                    ChainError::OtherPrimary => {
                        replaced = true;
                        chain.other_primary = true;
                    }
                    ChainError::PrimaryRefused(_) | ChainError::Other(_) => replaced = true,
                }
            }
        }
    }
    if changed {
        console(CHAIN_WARNING);
    } else if replaced {
        console(TPM_WARNING);
    } else if no_bank {
        console(NO_SHA256_BANK);
    }
    chain
}

fn halt_eof(what: &str) -> String {
    format!("end of input at the {what} prompt; boot is refused")
}

fn halt_failed(what: &str, error: &str) -> String {
    format!("the {what} prompt failed: {error}; boot is refused")
}

const REFUSED_ENTRY: &str = "the entry is longer than 256 bytes or holds a newline: enter it again";

/// One entry at `prompt`, a refused record asking again; end of input or
/// a failed applet is the halt reason.
fn entry(
    prompt: &mut dyn Prompt,
    text: &str,
    what: &str,
    console: &mut dyn FnMut(&str),
) -> Result<Entry, String> {
    loop {
        match prompt.read(text) {
            Ok(Entered::Line(entry)) => return Ok(entry),
            Ok(Entered::Refused) => console(REFUSED_ENTRY),
            Ok(Entered::Eof) => return Err(halt_eof(what)),
            Err(error) => return Err(halt_failed(what, &error)),
        }
    }
}

fn attempts(state: &td_tpm::DictionaryAttack) -> String {
    format!(
        "td: {} TPM PIN attempts remain before the TPM locks out",
        state.max_tries.saturating_sub(state.counter)
    )
}

/// The TPM PIN, while the TPM is not locked out: an entry the codec
/// refuses asks again with no TPM command, an empty one turns to another
/// way, and a wrong PIN asks again with the new count. `Ok(None)` is
/// another way; `Err` the halt reason. A release returns the PIN too, for
/// a selector update's pre-seal.
fn pin_loop<T: Tpm>(
    number: u8,
    token: &TpmPin,
    tpm: &mut T,
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Result<Option<(Secret, Pin)>, String> {
    let unreadable = |console: &mut dyn FnMut(&str), error: String| {
        console(&format!(
            "td: the TPM's dictionary-attack state cannot be read ({error}); use a security \
             key or the recovery key"
        ));
    };
    let mut state = match tpm.dictionary_attack() {
        Ok(state) => state,
        Err(error) => {
            unreadable(console, error);
            return Ok(None);
        }
    };
    if state.in_lockout {
        console(LOCKED_OUT);
        return Ok(None);
    }
    console(PIN_KEYS);
    console(&attempts(&state));
    loop {
        let typed = entry(prompt, PIN_PROMPT, "TPM PIN", console)?;
        if typed.as_bytes().is_empty() {
            return Ok(None);
        }
        let parsed = Pin::parse(typed.as_bytes());
        drop(typed);
        let pin = match parsed {
            Ok(pin) => pin,
            Err(error) => {
                console(&format!("{error}: enter it again"));
                continue;
            }
        };
        let auth = pin.auth_value(&token.salt);
        match tpm.unseal_pin(token, &auth) {
            Ok(secret) => {
                console(&format!("td token {number} (tpm-pin) released"));
                return Ok(Some((secret, pin)));
            }
            Err(error) => match error.authorization() {
                Some(AuthRefusal::AuthFail) => {
                    state = match tpm.dictionary_attack() {
                        Ok(state) => state,
                        Err(error) => {
                            unreadable(console, error);
                            return Ok(None);
                        }
                    };
                    if state.in_lockout {
                        console("td: wrong PIN");
                        console(LOCKED_OUT);
                        return Ok(None);
                    }
                    console("td: wrong PIN");
                    console(&attempts(&state));
                }
                Some(AuthRefusal::Lockout) => {
                    console(LOCKED_OUT);
                    return Ok(None);
                }
                None => {
                    console(&format!(
                        "td: the TPM PIN cannot release: {error}; use a security key or the \
                         recovery key"
                    ));
                    return Ok(None);
                }
            },
        }
    }
}

/// What another way needs: the header's fido2 tokens and the FIDO2 client.
struct Ways<'a, F> {
    header: &'a Header,
    fido: &'a mut F,
    hinted: bool,
}

/// A security key or the recovery key, without limit: an empty entry runs
/// the FIDO2 client, whose refusals are console lines that ask again; any
/// other entry goes to the recovery-key codec, whose refusals ask again.
/// `Err` is the halt reason.
fn another_way<F: Fido>(
    ways: &mut Ways<'_, F>,
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Result<(Opened, Key), String> {
    if !ways.hinted {
        console(ANOTHER_WAY_KEYS);
        ways.hinted = true;
    }
    loop {
        let typed = entry(
            prompt,
            ANOTHER_WAY_PROMPT,
            "security key or recovery key",
            console,
        )?;
        if typed.as_bytes().is_empty() {
            drop(typed);
            if let Some(released) = security_key(ways, prompt, console)? {
                return Ok(released);
            }
            continue;
        }
        let parsed = RecoveryKey::parse(typed.as_bytes());
        drop(typed);
        match parsed {
            Err(error) => console(&format!("{error}: enter it again")),
            Ok(_) if protected_plan::marker(ways.header).is_none() => console(
                "td: this volume holds no recovery-key marker, so the recovery key cannot be \
                 tried",
            ),
            Ok(key) => return Ok((Opened::RecoveryKey, Key::Recovery(key.passphrase()))),
        }
    }
}

/// One security key's release among the header's fido2 tokens naming a
/// keyslot.
fn security_key<F: Fido>(
    ways: &mut Ways<'_, F>,
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Result<Option<(Opened, Key)>, String> {
    let header = ways.header;
    let tokens: Vec<FidoToken<'_>> = header
        .protected
        .iter()
        .filter_map(|(number, token)| match token {
            Token::Fido2(fido) if fido.keyslot.is_some() => Some(FidoToken {
                number: *number,
                role: fido.role,
                credential: &fido.credential,
                salt: &fido.salt,
                x: &fido.x,
                y: &fido.y,
            }),
            _ => None,
        })
        .take(MAX_FIDO2_TOKENS)
        .collect();
    if tokens.is_empty() {
        console("td: this volume holds no security key");
        return Ok(None);
    }
    let Some(volume) = lockout::volume_uuid(&header.uuid) else {
        console("td: the volume's UUID is not one a security key can be asked for");
        return Ok(None);
    };
    let description = unlock_description(&header.uuid);
    match ways
        .fido
        .release(&tokens, &volume, &description, prompt, console)
    {
        Ok((index, secret)) => match tokens.get(index) {
            Some(token) => {
                console(&format!(
                    "td token {} ({}) released",
                    token.number,
                    token.role.name()
                ));
                Ok(Some((
                    Opened::Fido2 {
                        token: token.number,
                    },
                    Key::Secret(secret),
                )))
            }
            None => {
                console("td: the security key named no token of this volume");
                Ok(None)
            }
        },
        Err(FidoError::Retry(line)) => {
            console(&line);
            Ok(None)
        }
        Err(FidoError::Halt(reason)) => Err(reason),
    }
}

/// The reseal, offered where it can run: the planner admits one over the
/// header, PCRs 4 and 9 are measured on a SHA-256 bank, the pre-cap
/// lockout rule allows a seal (no lockout authorization another system
/// set without a lockout token here), and a lockout token is on the
/// header to anchor it. Confirmed, the new PIN twice, then
/// the seal and its verifying unseal, both before the cap. `Ok(None)` is
/// no reseal; `Err` the halt reason, which only a confirmed reseal's PIN
/// prompts give.
fn offer_reseal<T: Tpm>(
    header: &Header,
    opened: Opened,
    chain: &Chain,
    tpm: &mut T,
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Result<Option<NewPin>, String> {
    let placeholder = TpmPin {
        keyslot: None,
        pcrs: PinPcrs::Selector,
        primary: [0; PRIMARY_NAME_LEN],
        salt: [0; SALT_LEN],
        sealed: SealedObject {
            public: vec![0],
            private: vec![0],
        },
    };
    if let Err(refusal) = protected_plan::plan(header, opened, Some(&placeholder)) {
        console(&format!("no reseal is offered: {refusal}"));
        return Ok(None);
    }
    // A reseal requires the Name td recorded, so that a primary which
    // appeared since never receives a new authValue and secret: on a TPM
    // whose primary is another, the TPM PIN stays unavailable.
    if chain.other_primary {
        console(
            "no reseal is offered: this TPM's storage primary is not the one td recorded, and \
             td seals no new TPM PIN protector to another; security keys and the recovery key \
             remain",
        );
        return Ok(None);
    }
    // 8c names no PCR 7: whether Secure Boot is on is 8i's reading of the
    // event log.
    let policy = match tpm
        .open()
        .map_err(ObserveError::Tpm)
        .and_then(|mut client| observe_pin(&mut client, PinPcrs::Selector))
    {
        Ok(policy) => policy,
        Err(error) => {
            console(&format!("no reseal is offered: {error}"));
            return Ok(None);
        }
    };
    match tpm.dictionary_attack() {
        Ok(state) if state.lockout_auth_set && protected_plan::lockout(header).is_none() => {
            console(
                "no reseal is offered: td cannot prove this TPM's lockout authorization, which \
                 another system set: the TPM-primary shape is refused on this TPM until it is \
                 cleared from firmware; security keys and the recovery key remain",
            );
            return Ok(None);
        }
        Ok(_) => {}
        Err(error) => {
            console(&format!(
                "no reseal is offered: the TPM's lockout state cannot be read: {error}"
            ));
            return Ok(None);
        }
    }
    // A seal's keyslot is added only under the primary a lockout token
    // already on the header records: without one it would be discarded
    // after the cap, having sent the new PIN's authValue for nothing.
    if protected_plan::lockout(header).is_none() {
        console(NO_ANCHOR);
        return Ok(None);
    }
    console(RESEAL_WARNING);
    console(RESEAL_KEYS);
    let confirmed = match prompt.read(RESEAL_PROMPT) {
        Ok(Entered::Line(answer)) => answer.as_bytes() == RESEAL_WORD,
        Ok(Entered::Eof | Entered::Refused) => false,
        Err(error) => {
            console(&format!("the reseal question failed: {error}"));
            false
        }
    };
    if !confirmed {
        console("reseal declined: this boot opens once, and the TPM PIN protectors are unchanged");
        return Ok(None);
    }
    let pin = new_pin(prompt, console)?;
    Ok(seal_new(policy, chain.recorded, &pin, tpm, console))
}

/// Two entries the PIN codec admits that are equal.
fn new_pin(prompt: &mut dyn Prompt, console: &mut dyn FnMut(&str)) -> Result<Pin, String> {
    loop {
        let first = entry(prompt, NEW_PIN_PROMPT, "new PIN", console)?;
        let pin = match Pin::parse(first.as_bytes()) {
            Ok(pin) => pin,
            Err(error) => {
                console(&format!("{error}: enter the new PIN again"));
                continue;
            }
        };
        let second = entry(prompt, NEW_PIN_AGAIN_PROMPT, "new PIN", console)?;
        if first.as_bytes() == second.as_bytes() {
            return Ok(pin);
        }
        console("td: the two PINs differ: enter the new PIN again");
    }
}

/// A fresh secret sealed under `pin` to `policy`, before the cap, and
/// unsealed once with it, requiring the same secret. A reseal of a TPM
/// whose primary matched a recorded Name requires that Name again; one
/// over a TPM that never held td's object takes the primary as it finds
/// it. An object over the token's 128- and 256-byte bounds is refused
/// here, before the cap and before any keyslot or header write: such a
/// TPM is unusable for the TPM PIN.
fn seal_new<T: Tpm>(
    policy: PinPolicy,
    recorded: Option<[u8; PRIMARY_NAME_LEN]>,
    pin: &Pin,
    tpm: &mut T,
    console: &mut dyn FnMut(&str),
) -> Option<NewPin> {
    let refuse = |console: &mut dyn FnMut(&str), why: String| {
        console(&format!("no reseal: {why}; nothing is kept"));
        None
    };
    let mut salt = [0u8; SALT_LEN];
    if let Err(error) = read_random(Path::new(SALT_SOURCE), &mut salt, "PIN salt") {
        return refuse(console, error);
    }
    let secret = match Secret::generate() {
        Ok(secret) => secret,
        Err(error) => return refuse(console, error),
    };
    let auth = pin.auth_value(&salt);
    let sealed = match tpm.seal_pin(
        &policy,
        recorded.as_ref().map(|name| &name[..]),
        &auth,
        &secret,
    ) {
        Ok(sealed) => sealed,
        Err(error) => return refuse(console, format!("the seal failed: {error}")),
    };
    let (public, private) = (sealed.object.public.len(), sealed.object.private.len());
    if public > MAX_PIN_PUBLIC_BYTES || private > MAX_PIN_PRIVATE_BYTES {
        return refuse(
            console,
            format!(
                "this TPM's sealed object has {public}- and {private}-byte areas, over td's \
                 {MAX_PIN_PUBLIC_BYTES} and {MAX_PIN_PRIVATE_BYTES}: the TPM is unusable for \
                 the TPM PIN, and security keys and the recovery key remain"
            ),
        );
    }
    if protected_auth_policy(&sealed.object.public) != Ok(policy.digest()) {
        return refuse(console, "the TPM sealed another policy".into());
    }
    let Ok(primary) = <[u8; PRIMARY_NAME_LEN]>::try_from(sealed.primary_name.as_slice()) else {
        return refuse(
            console,
            "the TPM's storage primary Name is not SHA-256".into(),
        );
    };
    let token = TpmPin {
        keyslot: None,
        pcrs: policy.pcrs(),
        primary,
        salt,
        sealed: sealed.object,
    };
    match tpm.unseal_pin(&token, &auth) {
        Ok(verified) if td_tpm::equal(verified.expose(), secret.expose()) => {
            console("the new TPM PIN protector verifies before the cap");
            Some(NewPin { token, secret })
        }
        Ok(_) => refuse(console, "the new protector unsealed another secret".into()),
        Err(error) => refuse(
            console,
            format!("the new protector did not verify: {error}"),
        ),
    }
}

/// A selector update's pre-seal (ENCRYPTION.md "Selector update"): the
/// secret `token` released this boot, sealed under the PIN typed this boot
/// with a fresh salt to `predicted` and a literal-zero PCR 12, under the
/// storage primary the token records, before the cap. Its chain is not
/// running, so it cannot be unsealed to verify it; its bounds, its policy
/// and its primary are checked instead, and the object stays in RAM until
/// the commit imports it.
fn seal_predicted<T: Tpm>(
    token: &TpmPin,
    predicted: &Predicted,
    pin: &Pin,
    secret: &Secret,
    tpm: &mut T,
    console: &mut dyn FnMut(&str),
) -> Option<TpmPin> {
    let refuse = |console: &mut dyn FnMut(&str), why: String| {
        console(&format!(
            "no predicted protector for the staged update: {why}"
        ));
        None
    };
    let policy = match predicted
        .values(token.pcrs)
        .ok_or_else(|| "the token names a PCR td does not predict".to_string())
        .and_then(|values| PinPolicy::new(token.pcrs, &values))
    {
        Ok(policy) => policy,
        Err(error) => return refuse(console, error),
    };
    let mut salt = [0u8; SALT_LEN];
    if let Err(error) = read_random(Path::new(SALT_SOURCE), &mut salt, "PIN salt") {
        return refuse(console, error);
    }
    let auth = pin.auth_value(&salt);
    let sealed = match tpm.seal_pin(&policy, Some(&token.primary), &auth, secret) {
        Ok(sealed) => sealed,
        Err(error) => return refuse(console, format!("the seal failed: {error}")),
    };
    let (public, private) = (sealed.object.public.len(), sealed.object.private.len());
    if public > MAX_PIN_PUBLIC_BYTES || private > MAX_PIN_PRIVATE_BYTES {
        return refuse(
            console,
            format!(
                "the sealed object has {public}- and {private}-byte areas, over td's \
                 {MAX_PIN_PUBLIC_BYTES} and {MAX_PIN_PRIVATE_BYTES}"
            ),
        );
    }
    if protected_auth_policy(&sealed.object.public) != Ok(policy.digest()) {
        return refuse(console, "the TPM sealed another policy".into());
    }
    if sealed.primary_name != token.primary {
        return refuse(
            console,
            "the TPM sealed under another storage primary".into(),
        );
    }
    console("the staged update's predicted protector is sealed before the cap");
    Some(TpmPin {
        keyslot: None,
        pcrs: token.pcrs,
        primary: token.primary,
        salt,
        sealed: sealed.object,
    })
}

/// A selector update's token writes after the release (ENCRYPTION.md
/// "Selector update"): one confirmation of cryptsetup's metadata against
/// `header`, read afresh by the caller, then each step in order. Only
/// token imports and removals run; a step that needs a keyslot's key is
/// refused. `Err` names the step that failed, and later ones do not run.
pub fn write_tokens<C: Runner>(
    header: &Header,
    device: &Path,
    steps: &[Step],
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) -> Result<(), String> {
    if steps
        .iter()
        .any(|step| !matches!(step, Step::ImportToken { .. } | Step::RemoveToken { .. }))
    {
        return Err("a selector update writes tokens only".into());
    }
    let dump = cryptsetup
        .metadata(device)
        .map_err(|error| format!("cryptsetup's metadata cannot be read: {error}"))?;
    protected_plan::confirm(&header.metadata(), &dump).map_err(|refusal| refusal.to_string())?;
    let keys = Keys {
        opener: 0,
        opener_key: &[],
        new: None,
    };
    for step in steps {
        run_step(step, device, &keys, cryptsetup)
            .map_err(|error| format!("{} failed: {error}", describe_step(step)))?;
        console(&describe_step(step));
    }
    Ok(())
}

/// A tpm-pin object's salt is public, but drawn as a secret is.
const SALT_SOURCE: &str = "/dev/random";

/// The lockout's take or proof (ENCRYPTION.md "Lockout"), after the
/// keyslot's test, with the volume key: with lockoutAuthSet clear a take
/// under the verified token's `L`, or a fresh one imported unproven
/// first; with it set a proof under a verified token, SetPrimaryPolicy
/// before DictionaryAttackParameters. A refused proof removes an unproven
/// token and keeps a proven one. Anything else is a TPM td cannot prove,
/// named, and nothing further is sent to its lockout hierarchy. The
/// authorization and the primary's Name are returned when this boot holds
/// them proven.
fn lockout_stage<T: Tpm, C: Runner>(
    header: &Header,
    device: &Path,
    volume_key: &[u8],
    tpm: &mut T,
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) -> Option<(LockoutAuth, Vec<u8>)> {
    let cannot = |console: &mut dyn FnMut(&str), why: &str| {
        console(&format!(
            "td: td cannot prove this TPM's lockout authorization ({why}): the TPM-primary \
             shape is refused on this TPM until it is cleared from firmware; security keys and \
             the recovery key remain"
        ));
    };
    let Some(uuid) = lockout::volume_uuid(&header.uuid) else {
        console("no lockout step: the volume's UUID is not a LUKS2 UUID");
        return None;
    };
    let name = match tpm.primary_name() {
        Ok(name) => name,
        Err(error) => {
            console(&format!("no lockout step: the storage primary: {error}"));
            return None;
        }
    };
    let Ok(primary) = <[u8; PRIMARY_NAME_LEN]>::try_from(name.as_slice()) else {
        console("no lockout step: the storage primary's Name is not SHA-256");
        return None;
    };
    let state = match tpm.dictionary_attack() {
        Ok(state) => state,
        Err(error) => {
            console(&format!(
                "no lockout step: the TPM's lockout state: {error}"
            ));
            return None;
        }
    };
    let keys = Keys {
        opener: 0,
        opener_key: &[],
        new: None,
    };
    let mut write = |step: Step, console: &mut dyn FnMut(&str)| -> bool {
        let done = run_step(&step, device, &keys, cryptsetup);
        match &done {
            Ok(()) => console(&describe_step(&step)),
            Err(error) => console(&format!("{} failed: {error}", describe_step(&step))),
        }
        done.is_ok()
    };
    let token = protected_plan::lockout(header).map(|(number, token)| {
        (
            number,
            token,
            lockout::open(volume_key, &uuid, token, &primary),
        )
    });
    if !state.lockout_auth_set {
        let (number, auth, lockout_token) = match token {
            Some((number, token, Ok(auth))) => {
                console("the lockout token verifies: its authorization is taken again");
                (number, auth, token.clone())
            }
            // The verified token anchors every seal to the TPM it records:
            // a take here would re-anchor them to whatever primary answers.
            Some((_, _, Err(lockout::OpenError::OtherPrimary))) => {
                console(
                    "no lockout take: the header's lockout token names another TPM's storage \
                     primary, so this TPM was cleared, replaced or is being intercepted; \
                     protector management re-adds a TPM PIN protector under a security key \
                     or the recovery key",
                );
                return None;
            }
            _ => {
                let fresh = LockoutAuth::generate().and_then(|auth| {
                    lockout::seal(volume_key, &uuid, &auth, &primary).map(|token| (auth, token))
                });
                let (auth, token) = match fresh {
                    Ok(fresh) => fresh,
                    Err(error) => {
                        console(&format!("no lockout take: {error}"));
                        return None;
                    }
                };
                let (number, step) = match protected_plan::lockout_import(header, &token) {
                    Ok(import) => import,
                    Err(refusal) => {
                        console(&format!("no lockout take: {refusal}"));
                        return None;
                    }
                };
                // The authorization is on the header before the TPM holds it.
                if !write(step, console) {
                    console("no lockout take: its token is not on the header");
                    return None;
                }
                (number, auth, token)
            }
        };
        for command in [
            LockoutCommand::Take,
            LockoutCommand::Parameters,
            LockoutCommand::Reset,
        ] {
            if let Err(error) = tpm.lockout(command, &name, &auth) {
                console(&format!(
                    "the lockout take stopped: {error}; its token stays for the next boot's proof"
                ));
                return None;
            }
        }
        console(
            "td took the TPM's lockout hierarchy: 32 tries, one more every 600 s, a day after a \
             wrong lockout authorization, and the counter reset",
        );
        if !lockout_token.proven {
            if let Ok(step) = protected_plan::lockout_proven(number, &lockout_token) {
                write(step, console);
            }
        }
        return Some((auth, name));
    }
    let (number, lockout_token, auth) = match token {
        None => {
            cannot(
                console,
                "another system set it, and this volume holds no lockout token",
            );
            return None;
        }
        Some((_, _, Err(error))) => {
            cannot(console, &error.to_string());
            return None;
        }
        Some((number, token, Ok(auth))) => (number, token.clone(), auth),
    };
    // SetPrimaryPolicy under L: its success, authenticated by the
    // response HMAC under L, proves possession.
    match tpm.lockout(LockoutCommand::ClearPolicy, &name, &auth) {
        Ok(()) => {}
        Err(
            error @ LockoutError::Authorization {
                kind: AuthRefusal::AuthFail,
                ..
            },
        ) => {
            if !lockout_token.proven {
                write(protected_plan::lockout_remove(number), console);
            }
            cannot(console, &format!("the TPM refused its proof: {error}"));
            return None;
        }
        // A locked lockout hierarchy checked nothing: L may still be the
        // TPM's, so the token stays for a later boot's proof.
        Err(
            error @ LockoutError::Authorization {
                kind: AuthRefusal::Lockout,
                ..
            },
        ) => {
            console(&format!(
                "the lockout proof waits: {error}; the TPM's lockout hierarchy is locked out \
                 and checked nothing, so the lockout token stays"
            ));
            return None;
        }
        Err(LockoutError::OtherPrimary) => {
            cannot(console, "its storage primary is not the token's");
            return None;
        }
        Err(error) => {
            console(&format!("the lockout proof stopped: {error}"));
            return None;
        }
    }
    console("td proved the TPM's lockout authorization");
    if !lockout_token.proven {
        if let Ok(step) = protected_plan::lockout_proven(number, &lockout_token) {
            write(step, console);
        }
    }
    // Possession is proven: a refusal now, which an interposer can forge,
    // leaves the proven token and only stops this boot's lockout steps.
    if let Err(error) = tpm.lockout(LockoutCommand::Parameters, &name, &auth) {
        console(&format!(
            "td's dictionary-attack parameters were not set: {error}; the proven lockout token \
             stays, and the next boot's proof sets them"
        ));
        return None;
    }
    Some((auth, name))
}

/// The keys a plan's steps name: the opener's and the reseal's.
#[derive(Clone, Copy)]
struct Keys<'a> {
    opener: u8,
    opener_key: &'a [u8],
    new: Option<(u8, &'a Secret)>,
}

impl Keys<'_> {
    fn of(&self, slot: u8) -> io::Result<&[u8]> {
        if slot == self.opener && !self.opener_key.is_empty() {
            return Ok(self.opener_key);
        }
        match self.new {
            Some((new, secret)) if new == slot => Ok(secret.expose().as_slice()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no key for keyslot {slot}"),
            )),
        }
    }
}

fn describe_step(step: &Step) -> String {
    match step {
        Step::Test { slot } => format!("keyslot {slot} tested"),
        Step::KillSlot { slot, by } => {
            format!("keyslot {slot} destroyed, keyslot {by} authorizing")
        }
        Step::RemoveToken { token } => format!("token {token} removed"),
        Step::AddKeyslot { slot, by } => format!("keyslot {slot} added, keyslot {by} authorizing"),
        Step::ImportToken { token, id, replace } => {
            let (kind, role) = token.kind();
            let what = role.unwrap_or(kind);
            match (id, replace) {
                (Some(id), true) => format!("{what} token {id} replaced"),
                (Some(id), false) => format!("{what} token {id} imported"),
                (None, _) => format!("{what} token imported"),
            }
        }
    }
}

/// Run `steps` in order, reporting each commit; a failed step stops the
/// plan, and the next boot's plan completes what is left. Returns the
/// reseal's new keyslot once its test passed.
fn run_steps<C: Runner>(
    steps: &[Step],
    device: &Path,
    keys: &Keys<'_>,
    cryptsetup: &mut C,
    console: &mut dyn FnMut(&str),
) -> Option<u8> {
    let mut tested = None;
    let count = steps.len();
    for (index, step) in steps.iter().enumerate() {
        let at = index.saturating_add(1);
        match run_step(step, device, keys, cryptsetup) {
            Ok(()) => {
                console(&format!(
                    "protected plan step {at}/{count}: {}",
                    describe_step(step)
                ));
                if let Step::Test { slot } = step {
                    if keys.new.is_some_and(|(new, _)| new == *slot) {
                        tested = Some(*slot);
                    }
                }
            }
            Err(error) => {
                console(&format!(
                    "protected plan step {at}/{count} failed ({}): {error}",
                    describe_step(step)
                ));
                return tested;
            }
        }
    }
    tested
}

/// One step through the runner: a key on standard input by the keyslot it
/// opens, a token's JSON, or nothing; the reseal's secret of `AddKeyslot`
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
            json = token
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            json.as_bytes()
        }
        Input::Nothing => &[],
    };
    if let Step::AddKeyslot { slot, .. } = step {
        let secret = keys.of(*slot)?;
        let mut copy = [0u8; SECRET_LEN];
        let fits = copy.len() == secret.len();
        if fits {
            copy.copy_from_slice(secret);
        }
        let file = if fits {
            KeyFile::new(&copy)
        } else {
            Err(io::Error::other("the new keyslot's key is not a secret"))
        };
        td_tpm::zero(&mut copy);
        return cryptsetup.run(&step.args(device, file?.path()), input);
    }
    // Only `AddKeyslot`'s arguments name a key file.
    cryptsetup.run(&step.args(device, Path::new("")), input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cryptsetup::{Failed, MIN_KILL_KEY};
    use crate::luks2::{self, HeaderCopy};
    use crate::pin::{AuthValue, PROTECTED_ATTRIBUTES};
    use crate::protected::{Fido2, Lockout};
    use crate::tests::{live_secrets, Scripted};
    use crate::{cap_event, CAP_PCR};
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};
    use std::ffi::OsString;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::rc::Rc;
    use td_tpm::{
        put16, put32, put_blob, AuthSealed, DictionaryAttack, Refusal,
        UnsealError as TpmUnsealError, ALG_NULL, DICTIONARY_ATTACK_LOCK_RESET,
        DICTIONARY_ATTACK_PARAMETERS, HIERARCHY_CHANGE_AUTH, LOAD, POLICY_PCR, RC_AUTH_FAIL,
        RC_LOCKOUT, SET_PRIMARY_POLICY, SHA256, UNSEAL,
    };

    const DEVICE: &str = "/dev/vda2";
    const UUID: &str = "0f0e0d0c-0b0a-4908-8706-050403020100";
    const VOLUME_KEY: [u8; 64] = [0x77; 64];
    const RECOVERY: &[u8] = b"123459123459123459123459123459123459123459123459";
    const PIN: &[u8] = b"correct horse";
    const NEW_PIN: &[u8] = b"battery staple";
    const KEY_PIN: &[u8] = b"4321";
    const CRED_PRIMARY: &[u8] = &[0xc1; 16];
    const CRED_PRIMARY_2: &[u8] = &[0xc2; 16];
    const CRED_RECOVERY: &[u8] = &[0xc3; 16];
    const KEYED_HASH: u16 = 0x0008;

    type Log = Rc<RefCell<Vec<String>>>;

    fn note(log: &Log, entry: String) {
        log.borrow_mut().push(entry);
    }

    /// PCR 12 once the cap closed it.
    fn capped() -> [u8; 32] {
        td_tpm::digest(&[[0; 32], cap_event()].concat())
    }

    /// What the release model's TPM holds beyond the wire's PCRs, primary
    /// and Load: the salted sessions' commands td-tpm runs, modelled.
    struct State {
        da: DictionaryAttack,
        lockout_auth: Option<[u8; 32]>,
        primary: [u8; PRIMARY_NAME_LEN],
        /// Each sealed object's private area: its authValue and secret.
        objects: BTreeMap<Vec<u8>, ([u8; 32], [u8; SECRET_LEN])>,
        next: u32,
        /// A seal that fails, one over the token's bounds, or one whose
        /// object does not verify.
        seal_fault: Option<&'static str>,
        /// An interposer shows lockoutAuthSet clear.
        show_clear: bool,
        /// An interposer forwards the change and forges its refusal.
        forge_take_refusal: bool,
        /// The lockout hierarchy is itself locked out: every lockout
        /// authorization is refused unchecked.
        hierarchy_locked: bool,
        /// DictionaryAttackParameters is refused as a wrong
        /// authorization, as an interposer can forge after a success.
        refuse_parameters: bool,
        /// The TPM locks out between the dictionary-attack read and the
        /// Unseal.
        lockout_at_unseal: bool,
    }

    /// The TPM the release reaches: the scripted wire TPM for the chain
    /// check, the PCRs and the cap, and a model of td-tpm's salted
    /// operations that judges the PCRs on that wire.
    #[derive(Clone)]
    struct Model {
        wire: Scripted,
        state: Rc<RefCell<State>>,
        log: Log,
    }

    impl Model {
        fn new(log: &Log) -> Self {
            let wire = Scripted::new();
            let (_, name) = wire.client().storage_primary(None).unwrap();
            wire.codes();
            Self {
                wire,
                state: Rc::new(RefCell::new(State {
                    da: DictionaryAttack {
                        lockout_auth_set: false,
                        in_lockout: false,
                        counter: 0,
                        max_tries: 32,
                        interval: 600,
                        recovery: 86400,
                    },
                    lockout_auth: None,
                    primary: name.try_into().unwrap(),
                    objects: BTreeMap::new(),
                    next: 0,
                    seal_fault: None,
                    show_clear: false,
                    forge_take_refusal: false,
                    hierarchy_locked: false,
                    refuse_parameters: false,
                    lockout_at_unseal: false,
                })),
                log: log.clone(),
            }
        }

        fn reboot(&self) {
            self.wire.0.borrow_mut().pcrs[usize::from(CAP_PCR)] = [0; 32];
        }

        fn refused(command: u32, rc: u32) -> TpmUnsealError {
            TpmUnsealError {
                refusal: Some(Refusal { command, rc }),
                message: format!("TPM command {command:#x} refused: {rc:#x}"),
            }
        }
    }

    /// A tpm-pin object's public area as td-tpm writes it.
    fn pin_public(policy: &[u8; 32], unique: usize) -> Vec<u8> {
        let mut public = Vec::new();
        put16(&mut public, KEYED_HASH);
        put16(&mut public, SHA256);
        put32(&mut public, PROTECTED_ATTRIBUTES);
        put_blob(&mut public, policy).unwrap();
        put16(&mut public, ALG_NULL);
        put_blob(&mut public, &vec![0x33; unique]).unwrap();
        public
    }

    impl Tpm for Model {
        type Transport = Scripted;
        fn open(&mut self) -> Result<Client<Scripted>, String> {
            Ok(self.wire.client())
        }

        fn dictionary_attack(&mut self) -> Result<DictionaryAttack, String> {
            note(&self.log, "tpm da".into());
            let state = self.state.borrow();
            let mut da = state.da;
            if state.show_clear {
                da.lockout_auth_set = false;
            }
            Ok(da)
        }

        fn primary_name(&mut self) -> Result<Vec<u8>, String> {
            Ok(self.state.borrow().primary.to_vec())
        }

        fn seal_pin(
            &mut self,
            policy: &PinPolicy,
            expected: Option<&[u8]>,
            auth: &AuthValue,
            secret: &Secret,
        ) -> Result<AuthSealed, String> {
            note(&self.log, "tpm seal".into());
            assert_eq!(
                self.wire.pcr(usize::from(CAP_PCR)),
                [0; 32],
                "a seal after the cap"
            );
            let mut state = self.state.borrow_mut();
            let recorded = expected.is_some_and(|name| name == state.primary);
            note(&self.log, format!("tpm seal expects recorded {recorded}"));
            if expected.is_some_and(|name| name != state.primary) {
                return Err("another storage primary".into());
            }
            let unique = match state.seal_fault {
                Some("fail") => return Err("TPM command 0x153 refused: 0x902".into()),
                Some("oversize") => 92,
                _ => 32,
            };
            state.next += 1;
            let mut private = b"scripted:".to_vec();
            private.extend_from_slice(&state.next.to_be_bytes());
            if state.seal_fault != Some("verify") {
                state
                    .objects
                    .insert(private.clone(), (*auth.expose(), *secret.expose()));
            }
            Ok(AuthSealed {
                object: SealedObject {
                    public: pin_public(&policy.digest(), unique),
                    private,
                },
                primary_name: state.primary.to_vec(),
            })
        }

        fn unseal_pin(
            &mut self,
            token: &TpmPin,
            auth: &AuthValue,
        ) -> Result<Secret, TpmUnsealError> {
            note(&self.log, "tpm unseal".into());
            if self.state.borrow().lockout_at_unseal {
                self.state.borrow_mut().da.in_lockout = true;
            }
            if self.state.borrow().da.in_lockout {
                return Err(Self::refused(UNSEAL, RC_LOCKOUT));
            }
            let policy = release_pin_policy(&mut self.wire.client(), token.pcrs).unwrap();
            if self.wire.pcr(usize::from(CAP_PCR)) != [0; 32] {
                return Err(Self::refused(POLICY_PCR, 0x1c4));
            }
            let mut state = self.state.borrow_mut();
            if token.primary != state.primary {
                return Err(TpmUnsealError {
                    refusal: None,
                    message: "another storage primary".into(),
                });
            }
            let Some((sealed_auth, secret)) = state
                .objects
                .get(&token.sealed.private)
                .filter(|_| !self.wire.0.borrow().cleared)
                .copied()
            else {
                return Err(Self::refused(LOAD, 0x1df));
            };
            if protected_auth_policy(&token.sealed.public) != Ok(policy.digest()) {
                return Err(Self::refused(UNSEAL, 0x99d));
            }
            if sealed_auth != *auth.expose() {
                state.da.counter += 1;
                state.da.in_lockout = state.da.counter >= state.da.max_tries;
                return Err(Self::refused(UNSEAL, RC_AUTH_FAIL));
            }
            Ok(Secret::copy_of(&secret))
        }

        fn lockout(
            &mut self,
            command: LockoutCommand,
            primary: &[u8],
            auth: &LockoutAuth,
        ) -> Result<(), LockoutError> {
            note(&self.log, format!("tpm lockout {command:?}"));
            assert!(
                self.log
                    .borrow()
                    .iter()
                    .any(|entry| entry.starts_with("cs test") && entry.ends_with("ok")),
                "the lockout hierarchy before the keyslot's test"
            );
            assert_eq!(self.wire.pcr(usize::from(CAP_PCR)), capped());
            let mut state = self.state.borrow_mut();
            if primary != state.primary {
                return Err(LockoutError::OtherPrimary);
            }
            let code = match command {
                LockoutCommand::Take => HIERARCHY_CHANGE_AUTH,
                LockoutCommand::ClearPolicy => SET_PRIMARY_POLICY,
                LockoutCommand::Parameters => DICTIONARY_ATTACK_PARAMETERS,
                LockoutCommand::Reset => DICTIONARY_ATTACK_LOCK_RESET,
            };
            let refused = Err(LockoutError::Authorization {
                command: code,
                kind: AuthRefusal::AuthFail,
            });
            if command == LockoutCommand::Take {
                if state.da.lockout_auth_set {
                    if state.show_clear {
                        return refused;
                    }
                    return Err(LockoutError::AlreadySet);
                }
                state.da.lockout_auth_set = true;
                state.lockout_auth = Some(*auth.bytes());
                if state.forge_take_refusal {
                    return refused;
                }
                return Ok(());
            }
            if state.hierarchy_locked {
                return Err(LockoutError::Authorization {
                    command: code,
                    kind: AuthRefusal::Lockout,
                });
            }
            if state.lockout_auth != Some(*auth.bytes())
                || (command == LockoutCommand::Parameters && state.refuse_parameters)
            {
                return refused;
            }
            match command {
                LockoutCommand::Parameters => {
                    state.da.max_tries = lockout::DA_MAX_TRIES;
                    state.da.interval = lockout::DA_RECOVERY_SECS;
                    state.da.recovery = lockout::DA_LOCKOUT_RECOVERY_SECS;
                }
                LockoutCommand::Reset => {
                    state.da.counter = 0;
                    state.da.in_lockout = false;
                }
                _ => {}
            }
            Ok(())
        }
    }

    fn exited(code: i32) -> io::Error {
        Failed::new(
            Path::new("/bin/cryptsetup"),
            "open",
            ExitStatus::from_raw(code << 8),
        )
        .into_error()
    }

    /// A LUKS2 header as cryptsetup keeps it, and the cryptsetup commands
    /// run on it. Every command requires PCR 12 capped when a TPM exists,
    /// and every state it leaves must pass the reader's bounds.
    struct Disk {
        keyslots: BTreeMap<u8, Vec<u8>>,
        tokens: BTreeMap<u8, Token>,
        wire: Option<Scripted>,
        log: Log,
        commands: usize,
        /// The command of this index this boot fails, as a cut would.
        cut: Option<usize>,
        dump_override: Option<Vec<u8>>,
    }

    fn strip(token: &mut Token) {
        match token {
            Token::TpmPin(pin) => pin.keyslot = None,
            Token::Fido2(fido) => fido.keyslot = None,
            Token::RecoveryKey { keyslot } => *keyslot = None,
            _ => {}
        }
    }

    /// The keyslots, and each token as (number, kind, keyslot).
    type Shaped = (Vec<u8>, Vec<(u8, &'static str, Option<u8>)>);

    impl Disk {
        fn header(&self) -> Header {
            Header {
                copy: HeaderCopy::Primary,
                seqid: 1,
                hdr_size: 0x4000,
                uuid: UUID.into(),
                label: "td-system".into(),
                keyslots: self.keyslots.keys().copied().collect(),
                tokens: Vec::new(),
                orphans: Vec::new(),
                foreign: Vec::new(),
                protected: self
                    .tokens
                    .iter()
                    .map(|(number, token)| (*number, token.clone()))
                    .collect(),
            }
        }

        fn dump(&self) -> Vec<u8> {
            let keyslots: Vec<String> = self
                .keyslots
                .keys()
                .map(|slot| format!("\"{slot}\": {{\"type\": \"luks2\"}}"))
                .collect();
            let tokens: Vec<String> = self
                .tokens
                .iter()
                .map(|(number, token)| format!("\"{number}\": {}", token.encode().unwrap()))
                .collect();
            format!(
                "{{\"keyslots\": {{{}}}, \"tokens\": {{{}}}, \"segments\": {{}}, \"digests\": {{}}, \"config\": {{}}}}",
                keyslots.join(", "),
                tokens.join(", ")
            )
            .into_bytes()
        }

        fn opens(&self, slot: u8, key: &[u8]) -> bool {
            self.keyslots.get(&slot).is_some_and(|k| k == key)
        }

        /// Each keyslot, and each token as (number, kind, keyslot).
        fn shape(&self) -> Shaped {
            (
                self.keyslots.keys().copied().collect(),
                self.tokens
                    .iter()
                    .map(|(number, token)| {
                        (*number, token.kind().1.unwrap_or("?"), token.keyslot())
                    })
                    .collect(),
            )
        }
    }

    impl Runner for Disk {
        fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
            if let Some(wire) = &self.wire {
                assert_eq!(
                    wire.pcr(usize::from(CAP_PCR)),
                    capped(),
                    "a cryptsetup command before the cap: {args:?}"
                );
            }
            let words: Vec<String> = args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                words.iter().filter(|w| *w == DEVICE).count(),
                1,
                "{words:?}"
            );
            let after = |flag: &str| -> Option<u8> {
                let at = words.iter().position(|w| w == flag)?;
                Some(words[at + 1].parse().unwrap())
            };
            let index = self.commands;
            self.commands += 1;
            if self.cut == Some(index) {
                note(&self.log, format!("cs cut {}", words[0]));
                return Err(io::Error::other("cut"));
            }
            match (words[0].as_str(), words[1].as_str()) {
                ("open", "--test-passphrase") => {
                    let slot = after("--key-slot").unwrap();
                    if !self.opens(slot, input) {
                        note(&self.log, format!("cs test {slot} refused"));
                        return Err(exited(EXIT_BAD_PASSPHRASE));
                    }
                    note(&self.log, format!("cs test {slot} ok"));
                }
                ("luksAddKey", _) => {
                    let by = after("--key-slot").unwrap();
                    let slot = after("--new-key-slot").unwrap();
                    let key = std::fs::read(words.last().unwrap()).unwrap();
                    assert_eq!(key.len(), SECRET_LEN);
                    assert!(self.opens(by, input), "add {slot} by {by}");
                    assert!(!self.keyslots.contains_key(&slot));
                    self.keyslots.insert(slot, key);
                    note(&self.log, format!("cs add {slot} by {by}"));
                }
                ("token", "import") => {
                    let token = Token::decode(input).unwrap();
                    if let Some(slot) = token.keyslot() {
                        assert!(self.keyslots.contains_key(&slot));
                    }
                    let replace = words.iter().any(|w| w == "--token-replace");
                    let number = match after("--token-id") {
                        Some(id) => {
                            assert_eq!(self.tokens.contains_key(&id), replace, "{words:?}");
                            id
                        }
                        None => (0..32).find(|n| !self.tokens.contains_key(n)).unwrap(),
                    };
                    let kind = token.kind().1.unwrap_or("?");
                    self.tokens.insert(number, token);
                    note(&self.log, format!("cs import {number} {kind}"));
                }
                ("token", "remove") => {
                    let id = after("--token-id").unwrap();
                    assert!(self.tokens.remove(&id).is_some());
                    note(&self.log, format!("cs remove {id}"));
                }
                ("luksKillSlot", _) => {
                    let slot: u8 = words.last().unwrap().parse().unwrap();
                    assert!(input.len() >= MIN_KILL_KEY);
                    assert!(
                        self.keyslots
                            .keys()
                            .any(|other| *other != slot && self.opens(*other, input)),
                        "kill {slot} unauthorized"
                    );
                    assert!(self.keyslots.remove(&slot).is_some());
                    for token in self.tokens.values_mut() {
                        if token.keyslot() == Some(slot) {
                            strip(token);
                        }
                    }
                    note(&self.log, format!("cs kill {slot}"));
                }
                _ => panic!("unscripted cryptsetup {words:?}"),
            }
            luks2::metadata(&self.dump()).unwrap();
            Ok(())
        }

        fn metadata(&mut self, device: &Path) -> io::Result<Vec<u8>> {
            assert_eq!(device, Path::new(DEVICE));
            note(&self.log, "cs dump".into());
            Ok(self.dump_override.clone().unwrap_or_else(|| self.dump()))
        }
    }

    enum Say {
        Line(Vec<u8>),
        Eof,
        Refused,
        Fail,
    }

    fn say(bytes: &[u8]) -> Say {
        Say::Line(bytes.to_vec())
    }

    /// The consoles' secret-line, scripted: every prompt asked is logged,
    /// and the script must be used up.
    struct Script {
        items: VecDeque<Say>,
        log: Log,
    }

    impl Prompt for Script {
        fn read(&mut self, prompt: &str) -> Result<Entered, String> {
            note(&self.log, format!("ask {prompt}"));
            match self.items.pop_front() {
                Some(Say::Line(bytes)) => Ok(Entered::Line(Entry::new(&bytes))),
                Some(Say::Eof) => Ok(Entered::Eof),
                Some(Say::Refused) => Ok(Entered::Refused),
                Some(Say::Fail) => Err("secret-line failed".into()),
                None => panic!("unscripted prompt {prompt:?}: {:?}", self.log.borrow()),
            }
        }
    }

    /// The FIDO2 worker, scripted: the one plugged key, by credential, and
    /// its PIN.
    struct SecurityKeys {
        plugged: Option<(Vec<u8>, [u8; SECRET_LEN])>,
        log: Log,
    }

    impl Fido for SecurityKeys {
        fn release(
            &mut self,
            tokens: &[FidoToken<'_>],
            volume: &[u8; 16],
            description: &[u8],
            prompt: &mut dyn Prompt,
            console: &mut dyn FnMut(&str),
        ) -> Result<(usize, Secret), FidoError> {
            note(&self.log, "fido".into());
            assert_eq!(Some(*volume), lockout::volume_uuid(UUID));
            assert_eq!(description, unlock_description(UUID));
            let Some((credential, passphrase)) = &self.plugged else {
                return Err(FidoError::Retry("td: connect one security key".into()));
            };
            let Some(index) = tokens.iter().position(|t| t.credential == &credential[..]) else {
                return Err(FidoError::Retry(
                    "td: this security key is not enrolled for this volume".into(),
                ));
            };
            match prompt.read("td security key PIN (8 attempts left): ") {
                Ok(Entered::Line(pin)) if pin.as_bytes() == KEY_PIN => {
                    console("td: touch your security key");
                    Ok((index, Secret::copy_of(passphrase)))
                }
                Ok(Entered::Line(_)) => Err(FidoError::Retry("td: wrong security key PIN".into())),
                Ok(_) => Err(FidoError::Halt("end of input at the key's PIN".into())),
                Err(error) => Err(FidoError::Halt(error)),
            }
        }
    }

    fn fido_passphrase(slot: u8) -> [u8; SECRET_LEN] {
        [0xf0 | slot; SECRET_LEN]
    }

    fn fido_token(slot: Option<u8>, role: FidoRole, credential: &[u8]) -> Token {
        Token::Fido2(Fido2 {
            keyslot: slot,
            role,
            credential: credential.to_vec(),
            salt: [3; 32],
            x: [4; 32],
            y: [5; 32],
        })
    }

    #[derive(Debug, Default)]
    struct Run {
        released: Option<(u8, Method)>,
        finished: Option<Finished>,
        halt: Option<String>,
    }

    struct World {
        log: Log,
        model: Model,
        disk: Disk,
        keys: SecurityKeys,
        tpm: bool,
        /// What the selector's pre-seal predicts this boot.
        predict: Option<Predicted>,
    }

    impl World {
        fn empty() -> Self {
            let log: Log = Rc::default();
            let model = Model::new(&log);
            let mut disk = Disk {
                keyslots: BTreeMap::from([(0, RECOVERY.to_vec())]),
                tokens: BTreeMap::new(),
                wire: Some(model.wire.clone()),
                log: log.clone(),
                commands: 0,
                cut: None,
                dump_override: None,
            };
            disk.tokens
                .insert(0, Token::RecoveryKey { keyslot: Some(0) });
            let keys = SecurityKeys {
                plugged: None,
                log: log.clone(),
            };
            Self {
                log,
                model,
                disk,
                keys,
                tpm: true,
                predict: None,
            }
        }

        /// The TPM-primary shape: the recovery key in keyslot 0 and its
        /// marker as token 0, the tpm-pin protector under `PIN` in keyslot
        /// 1 as token 1, and a fido2-recovery token 2 on keyslot 2.
        fn tpm_shape() -> Self {
            let mut world = Self::empty();
            world.enroll_pin(1, 1, PIN);
            world.enroll_fido(2, 2, FidoRole::Recovery, CRED_RECOVERY);
            world
        }

        /// The FIDO2-primary shape: fido2-primary tokens 1 and 2 on
        /// keyslots 1 and 2, a fido2-recovery token 3 on keyslot 3.
        fn fido_shape() -> Self {
            let mut world = Self::empty();
            world.enroll_fido(1, 1, FidoRole::Primary, CRED_PRIMARY);
            world.enroll_fido(2, 2, FidoRole::Primary, CRED_PRIMARY_2);
            world.enroll_fido(3, 3, FidoRole::Recovery, CRED_RECOVERY);
            world
        }

        fn enroll_pin(&mut self, number: u8, slot: u8, pin: &[u8]) -> TpmPin {
            let policy = observe_pin(&mut self.model.wire.client(), PinPcrs::Selector).unwrap();
            let salt = [number; SALT_LEN];
            let auth = Pin::parse(pin).ok().unwrap().auth_value(&salt);
            let secret = Secret::generate().unwrap();
            let sealed = self.model.seal_pin(&policy, None, &auth, &secret).unwrap();
            let token = TpmPin {
                keyslot: Some(slot),
                pcrs: PinPcrs::Selector,
                primary: sealed.primary_name.try_into().unwrap(),
                salt,
                sealed: sealed.object,
            };
            self.disk.keyslots.insert(slot, secret.expose().to_vec());
            self.disk
                .tokens
                .insert(number, Token::TpmPin(token.clone()));
            self.log.borrow_mut().clear();
            token
        }

        fn enroll_fido(&mut self, number: u8, slot: u8, role: FidoRole, credential: &[u8]) {
            self.disk
                .keyslots
                .insert(slot, fido_passphrase(slot).to_vec());
            self.disk
                .tokens
                .insert(number, fido_token(Some(slot), role, credential));
        }

        fn plug(&mut self, credential: &[u8], slot: u8) {
            self.keys.plugged = Some((credential.to_vec(), fido_passphrase(slot)));
        }

        /// The lockout token for `auth` under the volume key and this
        /// TPM's primary.
        fn lockout_token(&self, auth: &[u8; 32], proven: bool) -> Lockout {
            let auth = LockoutAuth::from_bytes(&mut auth.clone()).unwrap();
            let primary = self.model.state.borrow().primary;
            let mut token = lockout::seal(
                &VOLUME_KEY,
                &lockout::volume_uuid(UUID).unwrap(),
                &auth,
                &primary,
            )
            .unwrap();
            token.proven = proven;
            token
        }

        /// td's lockout under `auth`, taken on an earlier boot: its proven
        /// token as number 3.
        fn taken(&mut self, auth: &[u8; 32]) {
            let token = self.lockout_token(auth, true);
            self.disk.tokens.insert(3, Token::Lockout(token));
            let mut state = self.model.state.borrow_mut();
            state.da.lockout_auth_set = true;
            state.lockout_auth = Some(*auth);
        }

        /// One boot: PCR 12 back at zero, the release over the header as
        /// the reader returns it, and `finish` after a release. Every
        /// secret is gone once it returns and the script is used up.
        fn boot(&mut self, script: Vec<Say>) -> Run {
            self.model.reboot();
            self.log.borrow_mut().clear();
            self.disk.commands = 0;
            if self.disk.dump_override.is_none() {
                luks2::metadata(&self.disk.dump()).unwrap();
            }
            let header = self.disk.header();
            let before = live_secrets();
            let mut prompt = Script {
                items: script.into(),
                log: self.log.clone(),
            };
            let console_log = self.log.clone();
            let mut console = move |line: &str| note(&console_log, format!("say {line}"));
            let mut model = self.model.clone();
            let mut tpm = self.tpm.then_some(&mut model);
            let device = Path::new(DEVICE);
            let predict = self.predict;
            let preseal_log = self.log.clone();
            let mut preseal = move |_: &mut dyn FnMut(&str)| {
                note(&preseal_log, "preseal".into());
                predict
            };
            let outcome = release(
                &header,
                device,
                tpm.as_deref_mut(),
                &mut self.disk,
                &mut self.keys,
                &mut prompt,
                &mut preseal,
                &mut console,
            );
            let mut run = Run::default();
            match outcome {
                Outcome::Released(released) => {
                    note(&self.log, format!("released {}", released.keyslot));
                    run.released = Some((released.keyslot, released.method));
                    run.finished = Some(finish(
                        released,
                        &header,
                        device,
                        &VOLUME_KEY,
                        tpm,
                        &mut self.disk,
                        &mut console,
                    ));
                }
                Outcome::Halt(reason) => run.halt = Some(reason),
                Outcome::Failed(error) => panic!("failed: {error}: {:?}", self.log.borrow()),
            }
            assert!(
                prompt.items.is_empty(),
                "unasked script: {:?}",
                self.log.borrow()
            );
            assert_eq!(live_secrets(), before, "secrets left alive");
            for entry in self.log.borrow().iter() {
                for secret in [PIN, NEW_PIN, KEY_PIN, RECOVERY] {
                    let text = String::from_utf8_lossy(secret);
                    assert!(
                        !entry.starts_with("say") || !entry.contains(&*text),
                        "a secret on the console: {entry}"
                    );
                }
            }
            run
        }

        fn lines(&self) -> Vec<String> {
            self.log.borrow().clone()
        }

        fn at(&self, entry: &str) -> usize {
            let log = self.log.borrow();
            log.iter()
                .position(|e| e.starts_with(entry))
                .unwrap_or_else(|| panic!("no {entry:?} in {log:?}"))
        }

        fn has(&self, entry: &str) -> bool {
            self.log.borrow().iter().any(|e| e.starts_with(entry))
        }

        fn count(&self, entry: &str) -> usize {
            self.log
                .borrow()
                .iter()
                .filter(|e| e.starts_with(entry))
                .count()
        }

        /// Whether the header changed: any cryptsetup write.
        fn wrote(&self) -> bool {
            ["cs add", "cs import", "cs remove", "cs kill"]
                .iter()
                .any(|verb| self.has(verb))
        }

        fn tpm_lockout(&self) -> bool {
            self.has("tpm lockout")
        }
    }

    fn released(run: &Run, world: &World) -> (u8, Method) {
        run.released
            .unwrap_or_else(|| panic!("nothing released: {run:?}: {:?}", world.lines()))
    }

    const ANOTHER: &str = "ask td: connect one security key";
    const ASK_PIN: &str = "ask td PIN";

    #[test]
    fn a_pin_release_takes_the_lockout_after_the_keyslot_test_then_proves_it() {
        let mut world = World::tpm_shape();
        let run = world.boot(vec![say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert!(Method::TpmPin.admits());
        // The PIN prompt is the first thing asked, nothing on the disk
        // before it, and the attempts are counted.
        assert!(world.at(ASK_PIN) < world.at("cs test 1 ok"));
        assert!(world.has("say td: 32 TPM PIN attempts remain"));
        assert!(!world.has(&format!("say {CHAIN_WARNING}")));
        // The take: its token on the header first, after the keyslot's
        // test, the change, the parameters and the reset, then proven.
        let order = [
            "cs test 1 ok",
            "cs dump",
            "cs import 3 lockout",
            "tpm lockout Take",
            "tpm lockout Parameters",
            "tpm lockout Reset",
        ];
        let at: Vec<usize> = order.iter().map(|entry| world.at(entry)).collect();
        assert!(
            at.windows(2).all(|pair| pair[0] < pair[1]),
            "{:?}",
            world.lines()
        );
        assert_eq!(world.count("cs import 3 lockout"), 2);
        assert_eq!(world.count("tpm lockout Reset"), 1);
        assert_eq!(
            run.finished,
            Some(Finished {
                resealed: None,
                lockout: true,
                predicted: None,
            })
        );
        let Some(Token::Lockout(token)) = world.disk.tokens.get(&3).cloned() else {
            panic!("no lockout token")
        };
        assert!(token.proven);
        let primary = world.model.state.borrow().primary;
        let auth = lockout::open(
            &VOLUME_KEY,
            &lockout::volume_uuid(UUID).unwrap(),
            &token,
            &primary,
        )
        .unwrap();
        assert_eq!(world.model.state.borrow().lockout_auth, Some(*auth.bytes()));
        assert!(world.model.state.borrow().da.lockout_auth_set);

        // The next PIN boot proves it: SetPrimaryPolicy before the
        // parameters, no reset after a PIN release, and nothing written.
        let run = world.boot(vec![say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert!(world.at("tpm lockout ClearPolicy") < world.at("tpm lockout Parameters"));
        assert!(!world.has("tpm lockout Reset") && !world.has("tpm lockout Take"));
        assert!(world.has("say td proved the TPM's lockout authorization"));
        assert!(!world.wrote());
    }

    #[test]
    fn an_entry_the_codec_refuses_costs_no_tpm_command() {
        let mut world = World::tpm_shape();
        let run = world.boot(vec![say(b"12345"), say(b"caf\xc3\xa9 au lait"), say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert_eq!(world.count(ASK_PIN), 3);
        assert_eq!(world.count("tpm unseal"), 1);
        assert_eq!(world.model.state.borrow().da.counter, 0);
    }

    #[test]
    fn wrong_pins_count_and_the_counter_is_reported_after_the_test() {
        let mut world = World::tpm_shape();
        let run = world.boot(vec![say(b"wrong pin 1"), say(b"wrong pin 2"), say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert_eq!(world.count("say td: wrong PIN"), 2);
        let remain =
            ["32", "31", "30"].map(|n| world.at(&format!("say td: {n} TPM PIN attempts remain")));
        assert!(remain.windows(2).all(|pair| pair[0] < pair[1]));
        let counted = world.at("say td: the TPM counted 2 wrong PINs since its last reset");
        assert!(counted > world.at("cs test 1 ok"));
        // The first boot's take reset the counter; nothing else did.
        assert_eq!(world.model.state.borrow().da.counter, 0);

        // A wrong PIN on a later boot is counted and left to decay.
        let run = world.boot(vec![say(b"wrong pin 3"), say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert!(world.has("say td: the TPM counted 1 wrong PINs"));
        assert!(!world.has("tpm lockout Reset"));
        assert_eq!(world.model.state.borrow().da.counter, 1);
    }

    #[test]
    fn a_lockout_turns_to_another_way_and_its_release_resets_the_counter() {
        let mut world = World::tpm_shape();
        world.model.state.borrow_mut().da.counter = 31;
        world.plug(CRED_RECOVERY, 2);
        let run = world.boot(vec![say(b"wrong pin 1"), say(b""), say(KEY_PIN)]);
        assert_eq!(released(&run, &world), (2, Method::Fido2Recovery));
        assert!(!Method::Fido2Recovery.admits());
        assert!(world.has(&format!("say {LOCKED_OUT}")));
        assert_eq!(world.count(ASK_PIN), 1);
        assert!(world.at(&format!("say {LOCKED_OUT}")) < world.at(ANOTHER));
        // No reseal: the object passed its chain check.
        assert!(!world.has("ask Type reseal"));
        // A security-key boot plans nothing for the tpm-pin protector.
        assert!(world.disk.tokens.contains_key(&1));
        assert!(world.has("say td: the TPM counted 32 wrong PINs"));
        assert!(world.at("tpm lockout Take") > world.at("cs test 2 ok"));
        assert!(!world.model.state.borrow().da.in_lockout);
        // The next boot takes the PIN again; a TPM locked out at boot asks
        // none.
        let run = world.boot(vec![say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        world.model.state.borrow_mut().da.in_lockout = true;
        let run = world.boot(vec![say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(!world.has(ASK_PIN));
        assert_eq!(world.count("tpm lockout Reset"), 1);
        assert!(!world.model.state.borrow().da.in_lockout);
    }

    /// The warning comes before any other prompt, no PIN is asked and no
    /// object is unsealed.
    fn warned(world: &World, warning: &str) {
        let warned = world.at(&format!("say {warning}"));
        let first_ask = world.at("ask");
        assert!(warned < first_ask, "{:?}", world.lines());
        assert!(world.at(ANOTHER) == first_ask);
        assert!(!world.has(ASK_PIN));
        assert!(!world.has("tpm unseal"));
    }

    #[test]
    fn a_changed_chain_a_fresh_tpm_and_another_primary_warn_and_ask_no_pin() {
        // A changed chain: the reseal is offered, and declined.
        let mut world = changed();
        let run = world.boot(vec![say(RECOVERY), say(b"no")]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        warned(&world, CHAIN_WARNING);
        assert!(world.has("say reseal declined"));
        assert!(!world.has("tpm seal"));

        // A fresh TPM: a new seed, so its primary has another Name; no
        // reseal is offered.
        let mut world = World::tpm_shape();
        world.model.wire.0.borrow_mut().cleared = true;
        world.model.wire.0.borrow_mut().seed = 1;
        let (_, fresh) = world.model.wire.client().storage_primary(None).unwrap();
        world.model.state.borrow_mut().primary = fresh.try_into().unwrap();
        let run = world.boot(vec![say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        warned(&world, TPM_WARNING);
        assert!(world.has("say no reseal is offered: this TPM's storage primary"));
        assert!(!world.has("ask Type reseal") && !world.has("tpm seal"));

        // A Load refused under the recorded primary: the reseal is offered,
        // and declined.
        let mut world = World::tpm_shape();
        world.taken(&[0x42; 32]);
        world.model.wire.0.borrow_mut().cleared = true;
        let run = world.boot(vec![say(RECOVERY), say(b"")]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        warned(&world, TPM_WARNING);
        assert!(world.has("ask Type reseal"));

        // Another primary: no reseal is offered, since none may be sealed
        // to it.
        let mut world = World::tpm_shape();
        if let Some(Token::TpmPin(pin)) = world.disk.tokens.get_mut(&1) {
            pin.primary[PRIMARY_NAME_LEN - 1] ^= 1;
        }
        let run = world.boot(vec![say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        warned(&world, TPM_WARNING);
        assert!(world.has("say no reseal is offered: this TPM's storage primary"));
        assert!(!world.has("ask Type reseal"));
    }

    #[test]
    fn each_fido2_role_and_the_recovery_key_release() {
        // TPM shape: fido2-recovery and the recovery key, with no reseal
        // while the object passes; the counter reset after each.
        let mut world = World::tpm_shape();
        world.plug(CRED_RECOVERY, 2);
        let run = world.boot(vec![say(b""), say(b""), say(KEY_PIN)]);
        assert_eq!(released(&run, &world), (2, Method::Fido2Recovery));
        assert!(world.at("fido") > world.at(ANOTHER));
        assert!(world.at("cs test 2 ok") > world.at("say td: touch your security key"));
        let run = world.boot(vec![say(b""), say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(world.has("tpm lockout Reset"));

        // FIDO2-primary shape: each primary key admits, the recovery one
        // does not; no TPM PIN, no chain check, no lockout, no reseal.
        for (credential, slot, method) in [
            (CRED_PRIMARY, 1, Method::Fido2Primary),
            (CRED_PRIMARY_2, 2, Method::Fido2Primary),
            (CRED_RECOVERY, 3, Method::Fido2Recovery),
        ] {
            let mut world = World::fido_shape();
            world.plug(credential, slot);
            let run = world.boot(vec![say(b""), say(KEY_PIN)]);
            assert_eq!(released(&run, &world), (slot, method));
            assert!(!world.has(ASK_PIN) && !world.has("ask Type reseal"));
            assert!(!world.tpm_lockout() && !world.wrote());
            assert_eq!(world.model.wire.pcr(usize::from(CAP_PCR)), capped());
        }
        let mut world = World::fido_shape();
        let run = world.boot(vec![say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));

        // A key enrolled under none of the tokens, a wrong key PIN and no
        // key are console lines that ask again.
        let mut world = World::fido_shape();
        let run = world.boot(vec![say(b""), say(b""), say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert_eq!(world.count("say td: connect one security key"), 2);
        world.plug(&[0x99; 16], 1);
        let run = world.boot(vec![say(b""), say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(world.has("say td: this security key is not enrolled"));
        let mut world = World::fido_shape();
        world.plug(CRED_PRIMARY, 1);
        let run = world.boot(vec![say(b""), say(b"9999"), say(b""), say(KEY_PIN)]);
        assert_eq!(released(&run, &world), (1, Method::Fido2Primary));
        assert!(world.has("say td: wrong security key PIN"));
    }

    #[test]
    fn a_wrong_recovery_key_prompts_again_after_the_cap() {
        let mut world = World::tpm_shape();
        let other = RecoveryKey::generate().unwrap();
        let typed = other.display();
        let run = world.boot(vec![
            say(b""),
            say(b"not a key"),
            say(typed.expose()),
            say(RECOVERY),
        ]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert_eq!(world.count("cs test 0"), 2);
        let refused = world.at("cs test 0 refused");
        assert!(world.log.borrow()[refused + 1..]
            .iter()
            .any(|e| e.starts_with(ANOTHER)));
        assert_eq!(world.count(ANOTHER), 3);
        // Codec refusals cost nothing; no PIN is asked after the cap.
        assert_eq!(world.count(ASK_PIN), 1);
    }

    /// A changed chain on the TPM-primary shape; the recovery key and a
    /// confirmed reseal under `NEW_PIN`.
    fn reseal_script() -> Vec<Say> {
        vec![
            say(RECOVERY),
            say(b"reseal"),
            say(b"short"),
            say(NEW_PIN),
            say(b"battery stapler"),
            say(NEW_PIN),
            say(NEW_PIN),
        ]
    }

    /// A changed chain on the TPM-primary shape whose lockout td took on
    /// an earlier boot: its proven token, under the volume key, records
    /// this TPM's primary.
    fn changed() -> World {
        let mut world = World::tpm_shape();
        world.taken(&[0x42; 32]);
        world.model.wire.0.borrow_mut().pcrs[4] = [0x50; 32];
        world
    }

    #[test]
    fn a_confirmed_reseal_is_verified_before_the_cap_and_planned_after_the_take() {
        let mut world = changed();
        let run = world.boot(reseal_script());
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(world.has("say td: the two PINs differ"));
        // The re-seal requires the primary the tpm-pin token recorded.
        assert!(world.has("tpm seal expects recorded true"));
        // The seal and its verifying unseal before any cryptsetup command
        // (each of which requires the cap).
        let verified = world.at("say the new TPM PIN protector verifies before the cap");
        assert!(world.at("tpm seal") < world.at("tpm unseal"));
        assert!(verified < world.at("cs test 0 ok"));
        // The take before the reseal's plan, which adds, imports, tests,
        // then retires the old protector under the new keyslot's key.
        let order = [
            "tpm lockout ClearPolicy",
            "tpm lockout Parameters",
            "cs add 3 by 0",
            "cs import 4 tpm-pin",
            "cs test 3 ok",
            "cs kill 1",
            "cs remove 1",
            "tpm lockout Reset",
        ];
        let at: Vec<usize> = order.iter().map(|entry| world.at(entry)).collect();
        assert!(
            at.windows(2).all(|pair| pair[0] < pair[1]),
            "{:?}",
            world.lines()
        );
        assert_eq!(run.finished.unwrap().resealed, Some(3));
        assert_eq!(
            world.disk.shape(),
            (
                vec![0, 2, 3],
                vec![
                    (0, "recovery-key", Some(0)),
                    (2, "fido2-recovery", Some(2)),
                    (3, "lockout", None),
                    (4, "tpm-pin", Some(3)),
                ]
            )
        );
        // The next boot releases with the new PIN on the changed chain.
        let run = world.boot(vec![say(NEW_PIN)]);
        assert_eq!(released(&run, &world), (3, Method::TpmPin));
        assert!(!world.wrote());
    }

    #[test]
    fn a_reseal_declined_or_failing_at_its_seal_or_verification_keeps_nothing() {
        let mut world = changed();
        let run = world.boot(vec![say(RECOVERY), say(b"Reseal")]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(!world.has("ask td new PIN") && !world.has("tpm seal"));
        for answer in [Say::Eof, Say::Refused, Say::Fail] {
            let mut world = changed();
            let run = world.boot(vec![say(RECOVERY), answer]);
            assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
            assert!(world.has("say reseal declined"));
        }
        for (fault, line) in [
            ("fail", "say no reseal: the seal failed"),
            ("verify", "say no reseal: the new protector did not verify"),
            (
                "oversize",
                "say no reseal: this TPM's sealed object has 138- and",
            ),
        ] {
            let mut world = changed();
            world.model.state.borrow_mut().seal_fault = Some(fault);
            let run = world.boot(reseal_script());
            assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
            // Refused before the cap, before any keyslot add or tpm-pin
            // import.
            assert!(world.at(line) < world.at("cs test 0 ok"), "{fault}");
            assert!(!world.has("cs add") && !world.has("cs import 4"));
            assert_eq!(run.finished.unwrap().resealed, None);
            assert!(world.disk.tokens.contains_key(&1));
        }
        // End of input at a confirmed reseal's PIN halts with nothing kept.
        let mut world = changed();
        let run = world.boot(vec![say(RECOVERY), say(b"reseal"), Say::Eof]);
        assert!(run.halt.is_some() && run.released.is_none());
        assert!(!world.has("cs"));
    }

    #[test]
    fn the_pre_cap_lockout_rule_refuses_a_seal_and_a_tpm_td_cannot_prove() {
        let mut world = World::tpm_shape();
        world.model.wire.0.borrow_mut().pcrs[4] = [0x50; 32];
        {
            let mut state = world.model.state.borrow_mut();
            state.da.lockout_auth_set = true;
            state.lockout_auth = Some([0xee; 32]);
        }
        let run = world.boot(vec![say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(world.has("say no reseal is offered: td cannot prove"));
        assert!(!world.has("ask Type reseal") && !world.has("tpm seal"));
        // After the test: named, and nothing sent to the lockout hierarchy.
        assert!(world.at("say td: td cannot prove") > world.at("cs test 0 ok"));
        assert!(!world.tpm_lockout() && !world.wrote());
    }

    #[test]
    fn a_reseal_after_a_refused_take_discards_the_object_before_its_import() {
        // An unproven token an interrupted take left, its L not yet the
        // TPM's: the take reuses it.
        let mut world = World::tpm_shape();
        let token = world.lockout_token(&[0x42; 32], false);
        world.disk.tokens.insert(3, Token::Lockout(token));
        world.model.wire.0.borrow_mut().pcrs[4] = [0x50; 32];
        world.model.state.borrow_mut().forge_take_refusal = true;
        let run = world.boot(reseal_script());
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(world.has("say the resealed protector is discarded before its import"));
        assert!(!world.has("cs add") && !world.has("cs import 4"));
        // The refused change kept its unproven token for one proof, which
        // the next boot makes, since the TPM took L behind the forgery.
        let Some(Token::Lockout(token)) = world.disk.tokens.get(&3).cloned() else {
            panic!("no lockout token")
        };
        assert!(!token.proven);
        world.model.state.borrow_mut().forge_take_refusal = false;
        let run = world.boot(reseal_script());
        assert_eq!(run.finished.unwrap().resealed, Some(3));
        assert!(world.at("tpm lockout ClearPolicy") < world.at("tpm lockout Parameters"));
        assert!(matches!(world.disk.tokens.get(&3), Some(Token::Lockout(t)) if t.proven));
    }

    #[test]
    fn the_take_reuses_a_verified_token_and_replaces_a_forged_one() {
        // A verified token naming this primary: its L is reused and its
        // mark kept, with no import before the change.
        let mut world = World::tpm_shape();
        let token = world.lockout_token(&[0x42; 32], false);
        world.disk.tokens.insert(3, Token::Lockout(token));
        let run = world.boot(vec![say(PIN)]);
        assert!(run.finished.unwrap().lockout);
        assert_eq!(world.model.state.borrow().lockout_auth, Some([0x42; 32]));
        assert_eq!(world.count("cs import 3 lockout"), 1);
        assert!(world.at("tpm lockout Reset") < world.at("cs import 3 lockout"));

        // A token whose tag fails, or naming another primary: replaced by a
        // fresh one before the change; its L never reaches the TPM.
        for forge in ["tag", "primary"] {
            let mut world = World::tpm_shape();
            let mut token = world.lockout_token(&[0x42; 32], true);
            match forge {
                "tag" => token.tag[0] ^= 1,
                _ => token.primary[0] ^= 1,
            }
            world.disk.tokens.insert(3, Token::Lockout(token));
            let run = world.boot(vec![say(PIN)]);
            assert!(run.finished.unwrap().lockout, "{forge}");
            let taken = world.model.state.borrow().lockout_auth.unwrap();
            assert_ne!(taken, [0x42; 32]);
            assert!(world.at("cs import 3 lockout") < world.at("tpm lockout Take"));
        }
    }

    #[test]
    fn a_refused_proof_removes_an_unproven_token_and_keeps_a_proven_one() {
        for proven in [false, true] {
            let mut world = World::tpm_shape();
            let token = world.lockout_token(&[0x42; 32], proven);
            world.disk.tokens.insert(3, Token::Lockout(token));
            {
                let mut state = world.model.state.borrow_mut();
                state.da.lockout_auth_set = true;
                state.lockout_auth = Some([0x43; 32]);
            }
            let run = world.boot(vec![say(PIN)]);
            assert!(!run.finished.unwrap().lockout);
            assert_eq!(world.count("tpm lockout"), 1);
            assert!(world.has("say td: td cannot prove this TPM's lockout authorization"));
            assert_eq!(world.disk.tokens.contains_key(&3), proven);
            // The next boot finds no token, or a refused proven one, and
            // sends one more proof only for a token it keeps.
            let run = world.boot(vec![say(PIN)]);
            assert!(!run.finished.unwrap().lockout);
            assert_eq!(world.count("tpm lockout"), usize::from(proven));
        }
        // An interposer showing lockoutAuthSet clear: the change is
        // refused, the fresh token kept for one proof, which is refused
        // and removes it; then nothing more is sent.
        let mut world = World::tpm_shape();
        {
            let mut state = world.model.state.borrow_mut();
            state.da.lockout_auth_set = true;
            state.lockout_auth = Some([0x43; 32]);
            state.show_clear = true;
        }
        world.boot(vec![say(PIN)]);
        assert!(world.has("say the lockout take stopped"));
        assert!(world.disk.tokens.contains_key(&3));
        world.model.state.borrow_mut().show_clear = false;
        world.boot(vec![say(PIN)]);
        assert!(!world.disk.tokens.contains_key(&3));
        world.boot(vec![say(PIN)]);
        assert!(!world.tpm_lockout());
    }

    #[test]
    fn without_a_tpm_device_nothing_is_capped_asked_or_taken() {
        let mut world = World::tpm_shape();
        world.tpm = false;
        world.disk.wire = None;
        let run = world.boot(vec![say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(world.at(&format!("say {NO_TPM}")) < world.at("ask"));
        assert!(world.has("say PCR 12 is not capped"));
        assert_eq!(world.model.wire.pcr(usize::from(CAP_PCR)), [0; 32]);
        assert!(!world.has(ASK_PIN) && !world.has("ask Type reseal") && !world.wrote());
        let mut world = World::fido_shape();
        world.tpm = false;
        world.disk.wire = None;
        world.plug(CRED_PRIMARY, 1);
        let run = world.boot(vec![say(b""), say(KEY_PIN)]);
        assert_eq!(released(&run, &world), (1, Method::Fido2Primary));
        assert!(!world.has(&format!("say {NO_TPM}")));
    }

    #[test]
    fn end_of_input_or_a_failed_applet_halts_before_any_command() {
        for end in [Say::Eof, Say::Fail] {
            let mut world = World::tpm_shape();
            let run = world.boot(vec![end]);
            assert!(run.halt.is_some());
            assert!(!world.has("cs"));
        }
        let mut world = World::tpm_shape();
        let run = world.boot(vec![Say::Refused, say(b""), Say::Eof]);
        assert!(run.halt.is_some());
        assert!(world.has("say the entry is longer than 256 bytes"));
    }

    /// One boot of whatever the header now asks: the new PIN once a new
    /// tpm-pin protector for this chain is on it, else the recovery key and
    /// a reseal.
    fn next_boot(world: &mut World) -> Run {
        let policy = observe_pin(&mut world.model.wire.client(), PinPcrs::Selector).unwrap();
        let resealed = world.disk.tokens.values().any(|token| {
            matches!(token, Token::TpmPin(pin) if pin.keyslot.is_some()
                && protected_auth_policy(&pin.sealed.public) == Ok(policy.digest()))
        });
        if resealed {
            world.boot(vec![say(NEW_PIN)])
        } else {
            world.boot(reseal_script())
        }
    }

    #[test]
    fn every_cut_of_the_reseal_and_its_take_converges_on_the_next_boots() {
        let mut whole = changed();
        whole.boot(reseal_script());
        let commands = whole.disk.commands;
        let end = whole.disk.shape();
        assert!(commands >= 6);
        for cut in 1..commands {
            let mut world = changed();
            world.disk.cut = Some(cut);
            let run = world.boot(reseal_script());
            assert!(run.released.is_some(), "cut {cut}");
            world.disk.cut = None;
            for _ in 0..3 {
                next_boot(&mut world);
            }
            // The keyslot and token numbers differ by the cut; the shape
            // converges.
            let (keyslots, tokens) = world.disk.shape();
            let kinds: Vec<(&str, bool)> = tokens
                .iter()
                .map(|(_, kind, slot)| (*kind, slot.is_some()))
                .collect();
            assert_eq!(
                keyslots.len(),
                end.0.len(),
                "cut {cut}: {:?}",
                world.disk.shape()
            );
            assert_eq!(
                kinds,
                end.1
                    .iter()
                    .map(|(_, kind, slot)| (*kind, slot.is_some()))
                    .collect::<Vec<_>>(),
                "cut {cut}"
            );
            assert!(matches!(
                world.disk.tokens.values().find(|t| matches!(t, Token::Lockout(_))),
                Some(Token::Lockout(t)) if t.proven
            ));
            // Converged: another boot writes nothing.
            let run = next_boot(&mut world);
            assert_eq!(run.released.unwrap().1, Method::TpmPin, "cut {cut}");
            assert!(!world.wrote(), "cut {cut}: {:?}", world.lines());
        }
    }

    #[test]
    fn every_cut_of_a_pin_release_plan_converges() {
        // A superseded tpm-pin protector beside the one that releases.
        let build = || {
            let mut world = World::tpm_shape();
            world.model.wire.0.borrow_mut().pcrs[4] = [0x50; 32];
            world.enroll_pin(4, 3, NEW_PIN);
            world.model.wire.0.borrow_mut().pcrs[4] = [0x44; 32];
            world
        };
        let mut whole = build();
        whole.boot(vec![say(PIN)]);
        let commands = whole.disk.commands;
        let end = whole.disk.shape();
        for cut in 1..commands {
            let mut world = build();
            world.disk.cut = Some(cut);
            world.boot(vec![say(PIN)]);
            world.disk.cut = None;
            world.boot(vec![say(PIN)]);
            world.boot(vec![say(PIN)]);
            assert_eq!(world.disk.shape(), end, "cut {cut}");
            world.boot(vec![say(PIN)]);
            assert!(!world.wrote(), "cut {cut}");
        }
    }

    #[test]
    fn forged_headers_release_nothing_unverified_and_write_nothing() {
        // A tpm-pin object under a policy that is not td's: the chain
        // check refuses it and no PIN is sent to it.
        let mut world = World::tpm_shape();
        if let Some(Token::TpmPin(pin)) = world.disk.tokens.get_mut(&1) {
            pin.sealed.public = pin_public(&[0xab; 32], 32);
        }
        let run = world.boot(vec![say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        warned(&world, CHAIN_WARNING);

        // A tpm-pin token naming the fido2 keyslot, and one naming the
        // recovery keyslot: the planner refuses, nothing is written and
        // nothing is sent to the lockout hierarchy.
        for slot in [2, 0] {
            let mut world = World::tpm_shape();
            if let Some(Token::TpmPin(pin)) = world.disk.tokens.get_mut(&1) {
                pin.keyslot = Some(slot);
            }
            let run = world.boot(vec![say(PIN), say(RECOVERY)]);
            assert_eq!(released(&run, &world), (0, Method::RecoveryKey), "{slot}");
            assert!(world.has(&format!("cs test {slot} refused")));
            assert!(world.has("say no header change this boot"));
            assert!(!world.wrote() && !world.tpm_lockout());
        }

        // A lockout token whose tag fails on a TPM whose lockout is set:
        // td cannot prove it, and sends nothing.
        let mut world = World::tpm_shape();
        let mut token = world.lockout_token(&[0x42; 32], true);
        token.tag[5] ^= 1;
        world.disk.tokens.insert(3, Token::Lockout(token));
        {
            let mut state = world.model.state.borrow_mut();
            state.da.lockout_auth_set = true;
            state.lockout_auth = Some([0x42; 32]);
        }
        let run = world.boot(vec![say(PIN)]);
        assert!(!run.finished.unwrap().lockout);
        assert!(!world.tpm_lockout() && !world.wrote());

        // A fido2-primary token beside a tpm-pin one: the FIDO2 shape's
        // release, and no plan, take or write.
        let mut world = World::tpm_shape();
        world.enroll_fido(5, 5, FidoRole::Primary, CRED_PRIMARY);
        world.plug(CRED_PRIMARY, 5);
        let run = world.boot(vec![say(b""), say(KEY_PIN)]);
        assert_eq!(released(&run, &world), (5, Method::Fido2Primary));
        assert!(world
            .has("say no header change this boot: the LUKS2 header holds a fido2-primary token"));
        assert!(!world.wrote() && !world.tpm_lockout() && !world.has(ASK_PIN));

        // Bounds exceeded in what cryptsetup reports: three tpm-pin
        // tokens. The release reads its own header; the plan writes
        // nothing once cryptsetup's metadata does not confirm it.
        let mut world = World::tpm_shape();
        let mut tokens = world.disk.tokens.clone();
        tokens.insert(6, world.disk.tokens.get(&1).cloned().unwrap());
        tokens.insert(7, world.disk.tokens.get(&1).cloned().unwrap());
        let over = Disk {
            keyslots: world.disk.keyslots.clone(),
            tokens,
            wire: None,
            log: world.log.clone(),
            commands: 0,
            cut: None,
            dump_override: None,
        };
        world.disk.dump_override = Some(over.dump());
        let run = world.boot(vec![say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert!(world.has("say no header change this boot"));
        assert!(!world.wrote() && !world.tpm_lockout());

        // Neither a device-bound nor an upgrading header is released here.
        let mut world = World::tpm_shape();
        world.disk.tokens.insert(
            6,
            Token::Upgrade {
                shape: Shape::Tpm,
                from: [0; 32],
            },
        );
        let run = world.boot(vec![]);
        assert!(run.halt.is_some());
        assert!(world.lines().is_empty());
    }

    #[test]
    fn the_post_cap_check_sends_policy_pcr_alone() {
        let world = World::tpm_shape();
        let Some(Token::TpmPin(pin)) = world.disk.tokens.get(&1).cloned() else {
            panic!()
        };
        world.model.wire.codes();
        // Before the cap a PolicyPCR passes: the caller halts as on a
        // release.
        policy_pcr_alone(world.model.wire.client(), &pin).unwrap();
        crate::cap(&mut world.model.wire.client()).unwrap();
        world.model.wire.codes();
        assert!(matches!(
            policy_pcr_alone(world.model.wire.client(), &pin),
            Err(UnsealError::PolicyRefused(_))
        ));
        let codes = world.model.wire.codes();
        assert!(
            !codes.contains(&UNSEAL) && !codes.contains(&LOAD),
            "{codes:x?}"
        );
        assert!(codes.contains(&POLICY_PCR));
        world.model.wire.0.borrow_mut().no_sha256_bank = true;
        assert!(matches!(
            policy_pcr_alone(world.model.wire.client(), &pin),
            Err(UnsealError::NoSha256Bank)
        ));
    }

    /// The `Tpm` trait's td-tpm operations against the pinned swtpm: a
    /// tpm-pin seal under the recorded primary passes the chain check, a
    /// wrong PIN is counted, the right one releases, the lockout's take,
    /// parameters, reset and proof hold under L and refuse another, and
    /// after the cap the post-cap check's PolicyPCR is refused with
    /// nothing loaded.
    #[test]
    #[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
    fn emulator_protected_operations() {
        use crate::tests::emulator::{fresh_state, Emulator, Socket};
        struct Swtpm(Emulator);
        impl Tpm for Swtpm {
            type Transport = Socket;
            fn open(&mut self) -> Result<Client<Socket>, String> {
                Ok(self.0.client())
            }
        }
        let mut tpm = Swtpm(Emulator::start(&fresh_state("protected-release")));
        tpm.0.client().extend_pcr(4, &[0x44; 32]).unwrap();
        tpm.0.client().extend_pcr(9, &[0x49; 32]).unwrap();
        let policy = observe_pin(&mut tpm.0.client(), PinPcrs::Selector).unwrap();
        let salt = [7; SALT_LEN];
        let auth = Pin::parse(PIN).ok().unwrap().auth_value(&salt);
        let secret = Secret::generate().unwrap();
        let name = tpm.primary_name().unwrap();
        let sealed = tpm.seal_pin(&policy, Some(&name), &auth, &secret).unwrap();
        assert_eq!(sealed.primary_name, name);
        let mut other = name.clone();
        other[PRIMARY_NAME_LEN - 1] ^= 1;
        assert!(tpm.seal_pin(&policy, Some(&other), &auth, &secret).is_err());
        let token = TpmPin {
            keyslot: Some(1),
            pcrs: PinPcrs::Selector,
            primary: name.clone().try_into().unwrap(),
            salt,
            sealed: sealed.object,
        };
        assert!(token.sealed.public.len() <= MAX_PIN_PUBLIC_BYTES);
        assert!(token.sealed.private.len() <= MAX_PIN_PRIVATE_BYTES);
        chain_check(tpm.0.client(), token.pcrs, &token.primary, &token.sealed).unwrap();
        let wrong = Pin::parse(b"wrong pin").ok().unwrap().auth_value(&salt);
        match tpm.unseal_pin(&token, &wrong) {
            Err(error) => assert_eq!(error.authorization(), Some(AuthRefusal::AuthFail)),
            Ok(_) => panic!("a wrong PIN released"),
        }
        assert_eq!(tpm.dictionary_attack().unwrap().counter, 1);
        let released = tpm.unseal_pin(&token, &auth).ok().unwrap();
        assert_eq!(released.expose(), secret.expose());

        let auth_l = LockoutAuth::from_bytes(&mut [0x42; 32]).unwrap();
        let other_l = LockoutAuth::from_bytes(&mut [0x43; 32]).unwrap();
        for command in [
            LockoutCommand::Take,
            LockoutCommand::Parameters,
            LockoutCommand::Reset,
            LockoutCommand::ClearPolicy,
            LockoutCommand::Parameters,
        ] {
            tpm.lockout(command, &name, &auth_l).unwrap();
        }
        let state = tpm.dictionary_attack().unwrap();
        assert!(state.lockout_auth_set);
        assert_eq!(
            (
                state.counter,
                state.max_tries,
                state.interval,
                state.recovery
            ),
            (0, 32, 600, 86400)
        );
        assert!(matches!(
            tpm.lockout(LockoutCommand::Take, &name, &auth_l),
            Err(LockoutError::AlreadySet)
        ));
        assert!(matches!(
            tpm.lockout(LockoutCommand::ClearPolicy, &other, &auth_l),
            Err(LockoutError::OtherPrimary)
        ));
        assert_eq!(
            tpm.lockout(LockoutCommand::ClearPolicy, &name, &other_l)
                .unwrap_err()
                .authorization(),
            Some(AuthRefusal::AuthFail)
        );

        policy_pcr_alone(tpm.0.client(), &token).unwrap();
        crate::cap(&mut tpm.0.client()).unwrap();
        assert!(matches!(
            policy_pcr_alone(tpm.0.client(), &token),
            Err(UnsealError::PolicyRefused(_))
        ));
    }

    /// A key that opens nothing discards the reseal confirmed after it, so
    /// the next key's release installs nobody's protector.
    #[test]
    fn a_failed_keyslot_test_discards_the_pending_reseal() {
        let mut world = changed();
        let wrong = RecoveryKey::generate().unwrap().display();
        let mut script = vec![say(wrong.expose())];
        script.extend(reseal_script().into_iter().skip(1));
        script.push(say(RECOVERY));
        let run = world.boot(script);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(world.has("say the reseal confirmed after that key is discarded"));
        assert!(world.at("cs test 0 refused") < world.at("cs test 0 ok"));
        assert!(!world.has("cs add") && !world.has("cs import"));
        assert_eq!(run.finished.unwrap().resealed, None);
        assert!(world.disk.tokens.contains_key(&1));
    }

    /// A lockout hierarchy that is itself locked out checks nothing: the
    /// unproven token stays. A SetPrimaryPolicy under L that succeeds
    /// proves possession: the token is marked proven and kept whatever the
    /// parameters' answer.
    #[test]
    fn a_proof_keeps_its_token_unless_l_was_checked_and_refused() {
        let mut world = World::tpm_shape();
        let token = world.lockout_token(&[0x42; 32], false);
        world.disk.tokens.insert(3, Token::Lockout(token));
        {
            let mut state = world.model.state.borrow_mut();
            state.da.lockout_auth_set = true;
            state.lockout_auth = Some([0x42; 32]);
            state.hierarchy_locked = true;
        }
        let run = world.boot(vec![say(PIN)]);
        assert!(!run.finished.unwrap().lockout);
        assert!(world.has("say the lockout proof waits"));
        assert!(!world.has("say td: td cannot prove"));
        assert!(matches!(world.disk.tokens.get(&3), Some(Token::Lockout(t)) if !t.proven));
        assert!(!world.wrote());

        {
            let mut state = world.model.state.borrow_mut();
            state.hierarchy_locked = false;
            state.refuse_parameters = true;
        }
        let run = world.boot(vec![say(PIN)]);
        assert!(!run.finished.unwrap().lockout);
        assert!(world.at("tpm lockout ClearPolicy") < world.at("cs import 3 lockout"));
        assert!(world.has("say td's dictionary-attack parameters were not set"));
        assert!(matches!(world.disk.tokens.get(&3), Some(Token::Lockout(t)) if t.proven));
        assert!(!world.has("cs remove"));
    }

    /// Every seal's primary must be the Name a lockout token already on
    /// the header records under the volume key. With the tpm-pin tokens
    /// deleted, a first seal goes to whatever primary answers: under
    /// another one than the lockout token's, nothing is taken and no
    /// keyslot added; beside a lockout token whose tag fails, the take
    /// replaces it and no keyslot is added; with no lockout token no
    /// reseal is offered and the take writes the anchor; under the
    /// anchored primary the keyslot is added.
    #[test]
    fn a_seal_adds_its_keyslot_only_under_the_anchored_primary() {
        for case in ["interposed", "forged", "none", "anchored"] {
            let mut world = World::empty();
            world.enroll_fido(2, 2, FidoRole::Recovery, CRED_RECOVERY);
            match case {
                "interposed" => {
                    world.taken(&[0x42; 32]);
                    // Another primary, which shows lockoutAuthSet clear.
                    let mut state = world.model.state.borrow_mut();
                    state.primary[PRIMARY_NAME_LEN - 1] ^= 1;
                    state.da.lockout_auth_set = false;
                    state.lockout_auth = None;
                }
                "forged" => {
                    let mut token = world.lockout_token(&[0x42; 32], true);
                    token.tag[0] ^= 1;
                    world.disk.tokens.insert(3, Token::Lockout(token));
                }
                "anchored" => world.taken(&[0x42; 32]),
                _ => {}
            }
            let script = if case == "none" {
                vec![say(RECOVERY)]
            } else {
                vec![say(RECOVERY), say(b"reseal"), say(NEW_PIN), say(NEW_PIN)]
            };
            let run = world.boot(script);
            assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
            // No tpm-pin token: the warning, before any prompt.
            assert!(world.at(&format!("say {NO_PIN_PROTECTOR}")) < world.at("ask"));
            assert!(world.at(&format!("say {CHAIN_WARNING}")) < world.at("ask"));
            assert!(!world.has(ASK_PIN));
            let finished = run.finished.unwrap();
            let lines = world.lines();
            match case {
                "interposed" => {
                    assert!(world.has("say no lockout take: the header's lockout token names"));
                    assert!(!world.tpm_lockout() && !finished.lockout, "{lines:?}");
                    assert!(matches!(world.disk.tokens.get(&3), Some(Token::Lockout(_))));
                }
                "forged" => {
                    assert!(
                        world.has("tpm lockout Take") && finished.lockout,
                        "{lines:?}"
                    );
                    assert!(world.has(
                        "say the resealed protector is discarded before its import: no lockout token"
                    ));
                }
                "none" => {
                    assert!(world.has(&format!("say {NO_ANCHOR}")));
                    assert!(!world.has("ask Type reseal") && !world.has("tpm seal"));
                    assert!(
                        world.has("tpm lockout Take") && finished.lockout,
                        "{lines:?}"
                    );
                }
                _ => {}
            }
            if case == "anchored" {
                assert!(world.has("tpm seal expects recorded false"));
                assert_eq!(finished.resealed, Some(1), "{lines:?}");
            } else {
                assert!(!world.has("cs add") && !world.has("tpm-pin"), "{lines:?}");
                assert_eq!(finished.resealed, None);
            }
        }
        // The anchor the take wrote: the next boot's reseal is offered and
        // its keyslot added.
        let mut world = World::empty();
        world.enroll_fido(2, 2, FidoRole::Recovery, CRED_RECOVERY);
        world.boot(vec![say(RECOVERY)]);
        let run = world.boot(vec![
            say(RECOVERY),
            say(b"reseal"),
            say(NEW_PIN),
            say(NEW_PIN),
        ]);
        assert!(
            run.finished.unwrap().resealed.is_some(),
            "{:?}",
            world.lines()
        );
    }

    /// A TPM without a SHA-256 bank: its own line, no PIN asked, no reseal,
    /// and after the security key or recovery key no lockout step, since
    /// this boot's cap closed nothing.
    #[test]
    fn a_tpm_without_a_sha256_bank_asks_no_pin() {
        let mut world = World::tpm_shape();
        world.model.wire.0.borrow_mut().no_sha256_bank = true;
        // No SHA-256 bank holds a cap to require.
        world.disk.wire = None;
        let run = world.boot(vec![say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(world.at(&format!("say {NO_SHA256_BANK}")) < world.at("ask"));
        assert!(!world.has(ASK_PIN) && !world.has("ask Type reseal"));
        assert!(!world.tpm_lockout() && !world.wrote());
    }

    /// An unmeasured PCR 4 or 9 offers no reseal: `observe_pin` refuses it
    /// before anything is sealed.
    #[test]
    fn an_unmeasured_selector_pcr_offers_no_reseal() {
        for pcr in [4usize, 9] {
            let mut world = changed();
            world.model.wire.0.borrow_mut().pcrs[pcr] = [0; 32];
            let run = world.boot(vec![say(RECOVERY)]);
            assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
            assert!(world.has(&format!(
                "say no reseal is offered: PCR {pcr} is unmeasured"
            )));
            assert!(!world.has("tpm seal"));
            assert!(matches!(
                observe_pin(&mut world.model.wire.client(), PinPcrs::Selector),
                Err(ObserveError::Unmeasured { pcr: refused }) if usize::from(refused) == pcr
            ));
        }
    }

    /// A TPM that locks out between its dictionary-attack read and the
    /// Unseal answers TPM_RC_LOCKOUT, which turns to another way.
    #[test]
    fn a_lockout_at_the_unseal_turns_to_another_way() {
        let mut world = World::tpm_shape();
        world.model.state.borrow_mut().lockout_at_unseal = true;
        let run = world.boot(vec![say(PIN), say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert_eq!(world.count(ASK_PIN), 1);
        assert!(world.at(&format!("say {LOCKED_OUT}")) < world.at(ANOTHER));
    }

    /// The chain a staged update predicts: PCRs 4 and 9 its own, PCR 7 as
    /// the TPM reads it.
    fn prediction(world: &World) -> Predicted {
        Predicted {
            pcr4: [0x44; 32],
            pcr7: world.model.wire.pcr(7),
            pcr9: [0x99; 32],
        }
    }

    /// A PIN release with a staged update: the pre-seal runs after the
    /// release and seals the released secret, under the typed PIN with a
    /// fresh salt, to the prediction and the recorded primary, before the
    /// cap (the model refuses a seal after it); `finish` hands it on naming
    /// the keyslot that released. The commit's import beside the released
    /// token, and the next boot on the predicted chain releases through it
    /// with the same PIN while the old token's chain check fails, and its
    /// plan retires the old token, the keyslot staying.
    #[test]
    fn a_pin_release_seals_the_prediction_and_the_next_chain_releases_with_it() {
        let mut world = World::tpm_shape();
        world.taken(&[0x42; 32]);
        let predicted = prediction(&world);
        world.predict = Some(predicted);
        let run = world.boot(vec![say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert!(world.at("tpm unseal") < world.at("preseal"));
        assert!(world.at("preseal") < world.at("tpm seal"));
        assert!(world.at("tpm seal") < world.at("say PCR 12 release cap closed"));
        assert!(world.has("tpm seal expects recorded true"));
        let finished = run.finished.unwrap();
        let token = finished.predicted.unwrap();
        let Some(Token::TpmPin(old)) = world.disk.tokens.get(&1).cloned() else {
            panic!("no tpm-pin token");
        };
        assert_eq!(token.keyslot, Some(1));
        assert_eq!(token.pcrs, PinPcrs::Selector);
        assert_eq!(token.primary, old.primary);
        assert_ne!(token.salt, old.salt);
        let policy = PinPolicy::new(PinPcrs::Selector, &[predicted.pcr4, predicted.pcr9]).unwrap();
        assert_eq!(
            protected_auth_policy(&token.sealed.public),
            Ok(policy.digest())
        );

        // The commit's token write, through the runner's confirmation.
        let header = world.disk.header();
        let step = protected_plan::predicted_import(&header, 1, &token, &VOLUME_KEY).unwrap();
        let mut lines = Vec::new();
        write_tokens(
            &header,
            Path::new(DEVICE),
            &[step],
            &mut world.disk,
            &mut |line| lines.push(line.to_string()),
        )
        .unwrap();
        assert_eq!(lines, ["tpm-pin token imported"]);
        let pins: Vec<Option<u8>> = world
            .disk
            .tokens
            .values()
            .filter_map(|token| match token {
                Token::TpmPin(pin) => Some(pin.keyslot),
                _ => None,
            })
            .collect();
        assert_eq!(pins, [Some(1), Some(1)]);

        // The next boot runs the predicted chain.
        {
            let mut wire = world.model.wire.0.borrow_mut();
            wire.pcrs[4] = predicted.pcr4;
            wire.pcrs[9] = predicted.pcr9;
        }
        world.predict = None;
        let run = world.boot(vec![say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert!(world.has("say td token 1 (tpm-pin): "));
        assert!(!world.has(&format!("say {CHAIN_WARNING}")));
        assert!(world.has("cs remove 1"));
        let (slots, tokens) = world.disk.shape();
        assert!(slots.contains(&1));
        assert_eq!(
            tokens
                .iter()
                .filter(|(_, kind, _)| *kind == "tpm-pin")
                .map(|(_, _, slot)| *slot)
                .collect::<Vec<_>>(),
            [Some(1)]
        );
    }

    /// The predicted protector goes on to the commit only beside a lockout
    /// this boot took or proved whose token, on the header read at
    /// release, anchors its primary: a first take anchors the next boot's,
    /// and a token recording another primary takes no lockout step.
    #[test]
    fn a_prediction_without_the_lockout_anchor_is_discarded() {
        let mut world = World::tpm_shape();
        world.predict = Some(prediction(&world));
        let run = world.boot(vec![say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert!(world.has("tpm seal"));
        let finished = run.finished.unwrap();
        assert!(finished.lockout);
        assert_eq!(finished.predicted, None);
        assert!(world.has(
            "say the update's predicted protector is discarded: no lockout token on this header"
        ));

        let mut world = World::tpm_shape();
        world.taken(&[0x42; 32]);
        if let Some(Token::Lockout(token)) = world.disk.tokens.get_mut(&3) {
            token.primary[3] ^= 1;
        }
        world.predict = Some(prediction(&world));
        let run = world.boot(vec![say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        let finished = run.finished.unwrap();
        assert!(!finished.lockout);
        assert_eq!(finished.predicted, None);
        assert!(world
            .has("say the update's predicted protector is discarded: td holds no proven lockout"));
    }

    /// No staged update, or a failed prediction: the pre-seal asks and
    /// nothing is sealed. A security key's or the recovery key's release
    /// runs no pre-seal at all.
    #[test]
    fn only_a_pin_release_with_a_prediction_seals_ahead() {
        let mut world = World::tpm_shape();
        let run = world.boot(vec![say(PIN)]);
        assert_eq!(released(&run, &world), (1, Method::TpmPin));
        assert!(world.has("preseal"));
        assert!(!world.has("tpm seal"));
        assert_eq!(run.finished.unwrap().predicted, None);

        let mut world = World::tpm_shape();
        world.predict = Some(prediction(&world));
        let run = world.boot(vec![say(b""), say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(!world.has("preseal"));
        assert_eq!(run.finished.unwrap().predicted, None);
    }

    /// A PIN whose released secret does not open its keyslot: the
    /// prediction sealed from it is discarded with the key, and whatever
    /// releases next carries none.
    #[test]
    fn a_failed_keyslot_test_discards_the_prediction() {
        let mut world = World::tpm_shape();
        world.predict = Some(prediction(&world));
        world.disk.keyslots.insert(1, vec![0x5a; SECRET_LEN]);
        let run = world.boot(vec![say(PIN), say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(world.has("tpm seal"));
        assert!(world.has("say the update's predicted protector seals what does not open"));
        assert_eq!(run.finished.unwrap().predicted, None);
    }

    /// A seal the TPM refuses, or one over the token's bounds: no
    /// prediction is kept, and the release stands.
    #[test]
    fn a_refused_seal_keeps_no_prediction() {
        for fault in ["fail", "oversize"] {
            let mut world = World::tpm_shape();
            world.predict = Some(prediction(&world));
            world.model.state.borrow_mut().seal_fault = Some(fault);
            let run = world.boot(vec![say(PIN)]);
            assert_eq!(released(&run, &world), (1, Method::TpmPin), "{fault}");
            assert!(world.has("say no predicted protector for the staged update"));
            assert_eq!(run.finished.unwrap().predicted, None, "{fault}");
        }
        let mut world = World::tpm_shape();
        world.predict = Some(prediction(&world));
        if let Some(Token::TpmPin(pin)) = world.disk.tokens.get_mut(&1) {
            pin.primary[3] ^= 1;
        }
        // The chain check refuses a token recording another primary, so
        // nothing releases by PIN and nothing is sealed.
        let run = world.boot(vec![say(RECOVERY)]);
        assert_eq!(released(&run, &world), (0, Method::RecoveryKey));
        assert!(!world.has("tpm seal"));
    }

    /// The commit's token steps: the predicted import only beside the one
    /// tpm-pin token naming the released keyslot and a lockout token that
    /// verifies for its primary under the volume key; the retry's write
    /// replacing its own, and its removal; `write_tokens` refuses a step
    /// that needs a key, and a header cryptsetup does not confirm.
    #[test]
    fn the_update_token_steps_and_their_runner() {
        let mut world = World::tpm_shape();
        let Some(Token::TpmPin(pin)) = world.disk.tokens.get(&1).cloned() else {
            panic!("no tpm-pin token");
        };
        let new = TpmPin {
            keyslot: None,
            salt: [0x5e; SALT_LEN],
            ..pin.clone()
        };
        // No lockout token, then one under another volume key, or one
        // recording another primary: no anchor.
        assert_eq!(
            protected_plan::predicted_import(&world.disk.header(), 1, &new, &VOLUME_KEY),
            Err(protected_plan::Refusal::NotAnchored)
        );
        world.taken(&[0x42; 32]);
        let header = world.disk.header();
        assert_eq!(
            protected_plan::predicted_import(&header, 1, &new, &[0x55; 64]),
            Err(protected_plan::Refusal::NotAnchored)
        );
        let mut elsewhere = new.clone();
        elsewhere.primary[3] ^= 1;
        assert_eq!(
            protected_plan::predicted_import(&header, 1, &elsewhere, &VOLUME_KEY),
            Err(protected_plan::Refusal::NotAnchored)
        );
        assert_eq!(
            protected_plan::predicted_import(&header, 2, &new, &VOLUME_KEY),
            Err(protected_plan::Refusal::NotPredicted)
        );
        let Ok(Step::ImportToken {
            token: Token::TpmPin(imported),
            id: None,
            replace: false,
        }) = protected_plan::predicted_import(&header, 1, &new, &VOLUME_KEY)
        else {
            panic!("no import");
        };
        assert_eq!(imported.keyslot, Some(1));
        // Beside two tpm-pin tokens, as a cut commit leaves them.
        world.disk.tokens.insert(5, Token::TpmPin(pin.clone()));
        assert_eq!(
            protected_plan::predicted_import(&world.disk.header(), 1, &new, &VOLUME_KEY),
            Err(protected_plan::Refusal::NotPredicted)
        );
        world.disk.tokens.remove(&5);

        let retry = crate::protected::UpdateRetry {
            pair: [1; 32],
            tag: [2; 32],
        };
        assert_eq!(protected_plan::retry_remove(&header), None);
        let first = protected_plan::retry_write(&header, &retry).unwrap();
        assert_eq!(
            first,
            Step::ImportToken {
                token: Token::UpdateRetry(retry.clone()),
                id: Some(4),
                replace: false,
            }
        );
        world.model.wire.0.borrow_mut().pcrs[usize::from(CAP_PCR)] = capped();
        let mut lines = Vec::new();
        let mut console = |line: &str| lines.push(line.to_string());
        write_tokens(
            &header,
            Path::new(DEVICE),
            &[first],
            &mut world.disk,
            &mut console,
        )
        .unwrap();
        let header = world.disk.header();
        assert_eq!(protected_plan::update_retry(&header), Some((4, &retry)));
        assert_eq!(
            protected_plan::retry_write(&header, &retry),
            Ok(Step::ImportToken {
                token: Token::UpdateRetry(retry.clone()),
                id: Some(4),
                replace: true,
            })
        );
        let remove = protected_plan::retry_remove(&header).unwrap();
        assert_eq!(remove, Step::RemoveToken { token: 4 });
        // A key step, and a header that is not the disk's, run nothing.
        assert!(write_tokens(
            &header,
            Path::new(DEVICE),
            &[Step::Test { slot: 1 }],
            &mut world.disk,
            &mut console
        )
        .is_err());
        let mut stale = World::tpm_shape();
        stale.taken(&[0x42; 32]);
        let stale = stale.disk.header();
        assert!(write_tokens(
            &stale,
            Path::new(DEVICE),
            std::slice::from_ref(&remove),
            &mut world.disk,
            &mut console
        )
        .is_err());
        write_tokens(
            &header,
            Path::new(DEVICE),
            &[remove],
            &mut world.disk,
            &mut console,
        )
        .unwrap();
        assert_eq!(protected_plan::update_retry(&world.disk.header()), None);
    }
}
