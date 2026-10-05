//! The pure transition planner (DESIGN.md "Transitions"): from the header
//! the reader returned and what released this boot, one transition's
//! ordered cryptsetup steps. It runs no cryptsetup and reaches no TPM; the
//! release orchestration executes the plan through the runner after the
//! cap, once cryptsetup's own metadata agrees with the header the plan was
//! computed from (`Plan::confirm`).

use crate::cryptsetup;
use crate::luks2::{self, Header, Metadata, MAX_TD_TOKENS};
use crate::token::{Role, Token, MAX_SLOT};
use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use td_tpm::SealedObject;

/// Always the recovery keyslot: no plan adds, kills or names it; a plan
/// tests it only as the key a recovery reseal opened the volume with.
pub const RECOVERY_SLOT: u8 = 0;

/// LUKS2 holds token numbers 0 to 31.
const TOKEN_NUMBERS: usize = MAX_SLOT as usize + 1;

/// What release produced, by the header's td tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Released {
    /// A device-bound protector released: the lowest-numbered such token.
    DeviceBound { token: u8 },
    /// The first-boot protector alone released: the lowest-numbered such
    /// token. The selector seals a device-bound protector before the cap.
    FirstBoot { token: u8 },
    /// No td token released: the recovery flow.
    Nothing,
}

/// The transition to plan, with the new protector's sealed object where
/// it adds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// The device-bound token `token` released and is kept.
    Keep { token: u8 },
    /// The first-boot token `token` alone released: `sealed` replaces it.
    FirstBoot { token: u8, sealed: SealedObject },
    /// Recovery opened the volume and its owner confirmed the reseal.
    Reseal { sealed: SealedObject },
}

/// One cryptsetup command of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `open --test-passphrase` on `slot` with its own key.
    Test { slot: u8 },
    /// `luksKillSlot` of `slot`, keyslot `by`'s key authorizing it.
    KillSlot { slot: u8, by: u8 },
    /// `token remove` of td token `token`.
    RemoveToken { token: u8 },
    /// `luksAddKey` of the new protector's keyslot `slot`, keyslot `by`'s
    /// key authorizing it and the new secret as its key file.
    AddKeyslot { slot: u8, by: u8 },
    /// `token import` of the new protector's token, as the lowest free
    /// token number.
    ImportToken { token: Token },
}

/// What a step's standard input carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input<'a> {
    /// The key of this keyslot: a released secret, the new protector's or
    /// the recovery passphrase.
    KeyOf(u8),
    /// This token's JSON.
    Token(&'a Token),
    Nothing,
}

impl Step {
    /// The step's exact arguments; `new_key` names the new secret's
    /// descriptor and appears only in `AddKeyslot`'s.
    pub fn args(&self, device: &Path, new_key: &Path) -> Vec<OsString> {
        match self {
            Self::Test { slot } => cryptsetup::test_args(device, *slot),
            Self::KillSlot { slot, .. } => cryptsetup::kill_slot_args(device, *slot),
            Self::RemoveToken { token } => cryptsetup::token_remove_args(device, *token),
            Self::AddKeyslot { slot, by } => cryptsetup::add_key_args(device, *by, *slot, new_key),
            Self::ImportToken { .. } => cryptsetup::token_import_args(device, None),
        }
    }

    pub fn input(&self) -> Input<'_> {
        match self {
            Self::Test { slot } => Input::KeyOf(*slot),
            Self::KillSlot { by, .. } | Self::AddKeyslot { by, .. } => Input::KeyOf(*by),
            Self::RemoveToken { .. } => Input::Nothing,
            Self::ImportToken { token } => Input::Token(token),
        }
    }
}

/// Why no plan runs this boot: each leaves the header as it is, and the
/// volume still opens with the released secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The header has no keyslot 0.
    NoRecoveryKeyslot,
    /// A td token names keyslot 0, which no td protector may hold.
    RecoveryKeyslotNamed { token: u8 },
    /// Two tokens, td or other, name one keyslot that a td token names.
    SharedKeyslot { slot: u8 },
    /// A released or transition token is not a td token naming a keyslot.
    UnknownToken { token: u8 },
    /// A transition token has the other role.
    WrongRole { token: u8, role: Role },
    /// Every keyslot from 1 to 31 is taken.
    NoFreeKeyslot,
    /// Every token number would still be taken when the new token is
    /// imported.
    NoFreeToken,
    /// The new protector's sealed object is outside the token format.
    Sealed(String),
    /// cryptsetup's own metadata could not be read or does not agree with
    /// the header the plan was computed from.
    Metadata(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRecoveryKeyslot => write!(f, "the LUKS2 header has no recovery keyslot 0"),
            Self::RecoveryKeyslotNamed { token } => {
                write!(f, "td token {token} names the recovery keyslot 0")
            }
            Self::SharedKeyslot { slot } => {
                write!(f, "keyslot {slot} is named by more than one token")
            }
            Self::UnknownToken { token } => {
                write!(f, "token {token} is not a td token naming a keyslot")
            }
            Self::WrongRole { token, role } => {
                write!(f, "td token {token} is {}", role.name())
            }
            Self::NoFreeKeyslot => write!(f, "the LUKS2 header has no free keyslot"),
            Self::NoFreeToken => write!(f, "the LUKS2 header has no free token number"),
            Self::Sealed(error) => write!(f, "the new protector: {error}"),
            Self::Metadata(error) => write!(f, "{error}"),
        }
    }
}

/// A plan and the header view it was computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    view: Metadata,
    steps: Vec<Step>,
}

impl Plan {
    /// The steps, for inspection. The executor runs only those `confirm`
    /// returns.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The steps to execute, once `metadata`, the output of `luksDump
    /// --dump-json-metadata` (`Cryptsetup::metadata`) taken before the
    /// first step, agrees with the view the plan was computed from: the
    /// same keyslots and the same tokens of every type. td's reader can use
    /// a checksum-valid copy that cryptsetup rejects (DESIGN.md "LUKS2
    /// tokens"); where the two disagree no step runs this boot.
    pub fn confirm(self, metadata: &[u8]) -> Result<Vec<Step>, Refusal> {
        let theirs = luks2::metadata(metadata).map_err(Refusal::Metadata)?;
        if theirs != self.view {
            return Err(Refusal::Metadata(
                "cryptsetup's LUKS2 metadata disagrees with the header copy td read".into(),
            ));
        }
        Ok(self.steps)
    }
}

/// The header's own rules, before any plan: keyslot 0 present and named by
/// no td token, and every td token's keyslot named by no other token.
fn check(header: &Header) -> Result<(), Refusal> {
    if !header.keyslots.contains(&RECOVERY_SLOT) {
        return Err(Refusal::NoRecoveryKeyslot);
    }
    let foreign = header.foreign_keyslots();
    for (number, token) in &header.tokens {
        let slot = token.keyslot();
        if slot == RECOVERY_SLOT {
            return Err(Refusal::RecoveryKeyslotNamed { token: *number });
        }
        let td = header
            .tokens
            .iter()
            .filter(|(_, other)| other.keyslot() == slot)
            .count();
        if td > 1 || foreign.contains(&slot) {
            return Err(Refusal::SharedKeyslot { slot });
        }
    }
    Ok(())
}

fn token(header: &Header, number: u8) -> Result<&Token, Refusal> {
    header
        .tokens
        .iter()
        .find(|(n, _)| *n == number)
        .map(|(_, token)| token)
        .ok_or(Refusal::UnknownToken { token: number })
}

/// Which transition `released`, the td token numbers whose secrets
/// released this boot, calls for: device-bound tokens before first-boot
/// ones, the lowest number of each. A number that is not a td token naming
/// a keyslot, an orphan's included, refuses, as does one naming keyslot 0,
/// which is never released. Whether the header admits a plan at all is
/// `plan`'s to say. When a plan's first test of the released keyslot
/// fails, the executor classifies again without that token.
pub fn classify(header: &Header, released: &[u8]) -> Result<Released, Refusal> {
    let mut device_bound = None;
    let mut first_boot = None;
    for number in released {
        let released = token(header, *number)?;
        if released.keyslot() == RECOVERY_SLOT {
            return Err(Refusal::RecoveryKeyslotNamed { token: *number });
        }
        let lowest = match released.role() {
            Role::DeviceBound => &mut device_bound,
            Role::FirstBoot => &mut first_boot,
        };
        *lowest = Some(lowest.map_or(*number, |lowest: u8| lowest.min(*number)));
    }
    Ok(match (device_bound, first_boot) {
        (Some(token), _) => Released::DeviceBound { token },
        (None, Some(token)) => Released::FirstBoot { token },
        (None, None) => Released::Nothing,
    })
}

/// The steps that retire `tokens`: each keyslot killed, then each token
/// removed, keyslot `by`'s key authorizing the kills.
fn retire(steps: &mut Vec<Step>, header: &Header, tokens: &[u8], by: u8) {
    for (number, token) in &header.tokens {
        if tokens.contains(number) {
            steps.push(Step::KillSlot {
                slot: token.keyslot(),
                by,
            });
        }
    }
    for number in tokens {
        steps.push(Step::RemoveToken { token: *number });
    }
}

/// One transition's ordered steps over `header`. No destroy precedes a
/// test of the key that authorizes it: the plan first tests the keyslot
/// the released token names, unless the recovery key opened the volume,
/// then removes every orphan, then commits the transition. A plan with
/// nothing to change is empty. A header that fails `check`, a td token
/// naming keyslot 0 among them, has no plan at all, orphans included, and
/// a plan that could not import its token refuses before any step.
pub fn plan(header: &Header, transition: &Transition) -> Result<Plan, Refusal> {
    check(header)?;
    let opener = match transition {
        Transition::Keep { token: number } => {
            let kept = token(header, *number)?;
            if kept.role() != Role::DeviceBound {
                return Err(Refusal::WrongRole {
                    token: *number,
                    role: kept.role(),
                });
            }
            kept.keyslot()
        }
        Transition::FirstBoot { token: number, .. } => {
            let released = token(header, *number)?;
            if released.role() != Role::FirstBoot {
                return Err(Refusal::WrongRole {
                    token: *number,
                    role: released.role(),
                });
            }
            released.keyslot()
        }
        Transition::Reseal { .. } => RECOVERY_SLOT,
    };
    // Orphans: a keyslot other than 0 no token of any type names, and a td
    // token naming none.
    let foreign = header.foreign_keyslots();
    let orphan_slots: Vec<u8> = header
        .keyslots
        .iter()
        .copied()
        .filter(|slot| {
            *slot != RECOVERY_SLOT
                && !foreign.contains(slot)
                && !header.tokens.iter().any(|(_, t)| t.keyslot() == *slot)
        })
        .collect();
    // The recovery key already opened the volume in keyslot 0.
    let mut steps = match transition {
        Transition::Reseal { .. } => Vec::new(),
        _ => vec![Step::Test { slot: opener }],
    };
    for slot in &orphan_slots {
        steps.push(Step::KillSlot {
            slot: *slot,
            by: opener,
        });
    }
    for (number, _) in &header.orphans {
        steps.push(Step::RemoveToken { token: *number });
    }
    let numbers = |keep: &dyn Fn(&Token) -> bool| -> Vec<u8> {
        header
            .tokens
            .iter()
            .filter(|(_, token)| keep(token))
            .map(|(number, _)| *number)
            .collect()
    };
    let done = |steps: Vec<Step>| Plan {
        view: header.metadata(),
        steps,
    };
    let (sealed, released, mut old) = match transition {
        Transition::Keep { token: kept } => {
            // A leftover first-boot token and every superseded one.
            let old = numbers(&|_| true)
                .into_iter()
                .filter(|number| number != kept)
                .collect::<Vec<_>>();
            retire(&mut steps, header, &old, opener);
            return Ok(done(if steps.len() == 1 { Vec::new() } else { steps }));
        }
        Transition::FirstBoot { sealed, token } => (
            sealed,
            Some(*token),
            numbers(&|token| token.role() == Role::FirstBoot),
        ),
        Transition::Reseal { sealed } => (sealed, None, numbers(&|_| true)),
    };
    let mut taken: Vec<u8> = header
        .keyslots
        .iter()
        .copied()
        .filter(|slot| !orphan_slots.contains(slot))
        .collect();
    let mut early = Vec::new();
    if header.tokens.len() >= MAX_TD_TOKENS {
        // At the bound the plan first retires every td token that did not
        // release, as far as it is told: all but the released first-boot
        // token, and in recovery every one. Keyslot 0 or the tested
        // keyslot still opens the volume.
        early = numbers(&|_| true)
            .into_iter()
            .filter(|number| Some(*number) != released)
            .collect::<Vec<_>>();
        taken.retain(|slot| {
            !header
                .tokens
                .iter()
                .any(|(number, t)| early.contains(number) && t.keyslot() == *slot)
        });
        old.retain(|number| !early.contains(number));
    }
    // The import takes the lowest free number of every type; the orphans
    // and the early retirements are removed before it.
    let held = header
        .token_numbers()
        .len()
        .checked_sub(header.orphans.len())
        .and_then(|held| held.checked_sub(early.len()))
        .ok_or(Refusal::NoFreeToken)?;
    if held >= TOKEN_NUMBERS {
        return Err(Refusal::NoFreeToken);
    }
    let slot = (1..=MAX_SLOT)
        .find(|slot| !taken.contains(slot))
        .ok_or(Refusal::NoFreeKeyslot)?;
    let new = Token::new(slot, Role::DeviceBound, sealed.clone()).map_err(Refusal::Sealed)?;
    retire(&mut steps, header, &early, opener);
    steps.push(Step::AddKeyslot { slot, by: opener });
    steps.push(Step::ImportToken { token: new });
    steps.push(Step::Test { slot });
    retire(&mut steps, header, &old, slot);
    Ok(done(steps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::luks2::HeaderCopy;
    use std::collections::BTreeMap;

    /// The recovery key's identity in the model.
    const RECOVERY_KEY: u8 = 0;

    /// A sealed object tagged with the boot chain it was sealed to and the
    /// key it seals.
    fn sealed(chain: u8, key: u8) -> SealedObject {
        SealedObject {
            public: vec![0x00, 0x08, chain, key],
            private: vec![0x01, chain],
        }
    }

    fn chain_of(sealed: &SealedObject) -> u8 {
        sealed.public[2]
    }

    fn key_of(sealed: &SealedObject) -> u8 {
        sealed.public[3]
    }

    /// One keyslot: the key that opens it, and whether a cut kill wiped
    /// its area while its JSON stayed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Slot {
        key: u8,
        dead: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Td { role: Role, chain: u8, key: u8 },
        Foreign,
    }

    /// A token: the keyslot it names, if any, and what it is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Entry {
        slot: Option<u8>,
        kind: Kind,
    }

    /// The header as cryptsetup keeps it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Model {
        keyslots: BTreeMap<u8, Slot>,
        tokens: BTreeMap<u8, Entry>,
    }

    /// The key keyslot `slot` was made with in a fixture.
    fn slot_key(slot: u8) -> u8 {
        if slot == RECOVERY_SLOT {
            RECOVERY_KEY
        } else {
            100 + slot
        }
    }

    /// A td token naming `slot`, sealing that keyslot's fixture key.
    fn td(slot: u8, role: Role, chain: u8) -> Entry {
        Entry {
            slot: Some(slot),
            kind: Kind::Td {
                role,
                chain,
                key: slot_key(slot),
            },
        }
    }

    fn orphan(role: Role) -> Entry {
        Entry {
            slot: None,
            kind: Kind::Td {
                role,
                chain: 0,
                key: 0,
            },
        }
    }

    fn foreign(slot: Option<u8>) -> Entry {
        Entry {
            slot,
            kind: Kind::Foreign,
        }
    }

    /// The keyslots and each token as (number, keyslot, role, chain), the
    /// keys and seals aside.
    type Shape = (Vec<u8>, Vec<(u8, Option<u8>, Option<Role>, u8)>);

    fn shape(keyslots: &[u8], tokens: &[(u8, Entry)]) -> Shape {
        Model::new(keyslots, tokens).shape()
    }

    impl Model {
        fn new(keyslots: &[u8], tokens: &[(u8, Entry)]) -> Self {
            Self {
                keyslots: keyslots
                    .iter()
                    .map(|slot| {
                        (
                            *slot,
                            Slot {
                                key: slot_key(*slot),
                                dead: false,
                            },
                        )
                    })
                    .collect(),
                tokens: tokens.iter().copied().collect(),
            }
        }

        fn shape(&self) -> Shape {
            (
                self.keyslots.keys().copied().collect(),
                self.tokens
                    .iter()
                    .map(|(number, entry)| match entry.kind {
                        Kind::Td { role, chain, .. } => (*number, entry.slot, Some(role), chain),
                        Kind::Foreign => (*number, entry.slot, None, 0),
                    })
                    .collect(),
            )
        }

        fn td_tokens(&self) -> usize {
            self.tokens
                .values()
                .filter(|entry| matches!(entry.kind, Kind::Td { .. }))
                .count()
        }

        /// What `luks2::read` returns for it.
        fn header(&self) -> Header {
            let mut tokens = Vec::new();
            let mut orphans = Vec::new();
            let mut foreign = Vec::new();
            for (number, entry) in &self.tokens {
                match (entry.kind, entry.slot) {
                    (Kind::Td { role, chain, key }, Some(slot)) => {
                        tokens.push((*number, Token::new(slot, role, sealed(chain, key)).unwrap()))
                    }
                    (Kind::Td { role, .. }, None) => orphans.push((*number, role)),
                    (Kind::Foreign, slot) => foreign.push((*number, slot.into_iter().collect())),
                }
            }
            Header {
                copy: HeaderCopy::Primary,
                seqid: 1,
                hdr_size: 0x4000,
                uuid: String::new(),
                label: String::new(),
                keyslots: self.keyslots.keys().copied().collect(),
                tokens,
                orphans,
                foreign,
            }
        }

        /// The model as `luksDump --dump-json-metadata` prints it,
        /// indented, with fields td does not read.
        fn dump(&self) -> Vec<u8> {
            let keyslots: Vec<String> = self
                .keyslots
                .keys()
                .map(|slot| format!("\"{slot}\": {{\n      \"type\": \"luks2\"\n    }}"))
                .collect();
            let header = self.header();
            let mut tokens = Vec::new();
            for (number, token) in &header.tokens {
                tokens.push(format!("\"{number}\": {}", token.encode()));
            }
            for (number, role) in &header.orphans {
                tokens.push(format!(
                    "\"{number}\": {{\"type\":\"td-protector\",\"keyslots\":[],\"role\":\"{}\",\"public\":\"00\",\"private\":\"00\"}}",
                    role.name()
                ));
            }
            for (number, slots) in &header.foreign {
                let slots: Vec<String> = slots.iter().map(|s| format!("\"{s}\"")).collect();
                tokens.push(format!(
                    "\"{number}\": {{\n      \"type\": \"systemd-tpm2\",\n      \"keyslots\": [{}]\n    }}",
                    slots.join(", ")
                ));
            }
            format!(
                "{{\n  \"keyslots\": {{\n    {}\n  }},\n  \"tokens\": {{\n    {}\n  }},\n  \"segments\": {{}},\n  \"digests\": {{}},\n  \"config\": {{\n    \"json_size\": \"12288\"\n  }}\n}}",
                keyslots.join(",\n    "),
                tokens.join(",\n    ")
            )
            .into_bytes()
        }

        fn live(&self, slot: u8, key: u8) -> bool {
            self.keyslots
                .get(&slot)
                .is_some_and(|s| !s.dead && s.key == key)
        }

        /// Applies one step with `key` on its standard input as cryptsetup
        /// would, refusing what it refuses; `new` is the new protector's
        /// key and `cut` stops a kill after it wiped the area and before it
        /// wrote the JSON.
        fn apply(
            &mut self,
            step: &Step,
            key: Option<u8>,
            new: u8,
            cut: bool,
        ) -> Result<(), String> {
            let key = || key.ok_or_else(|| format!("{step:?} without a key"));
            match step {
                Step::Test { slot } => {
                    if !self.live(*slot, key()?) {
                        return Err(format!("keyslot {slot} does not open with the key"));
                    }
                }
                Step::KillSlot { slot, by } => {
                    // cryptsetup tries the key on every other keyslot.
                    if slot == by || !self.keyslots.contains_key(slot) || !self.live(*by, key()?) {
                        return Err(format!("kill {slot} by {by}"));
                    }
                    if *slot == RECOVERY_SLOT {
                        return Err("killed the recovery keyslot".into());
                    }
                    if cut {
                        if let Some(s) = self.keyslots.get_mut(slot) {
                            s.dead = true;
                        }
                        return Ok(());
                    }
                    self.keyslots.remove(slot);
                    for entry in self.tokens.values_mut() {
                        if entry.slot == Some(*slot) {
                            entry.slot = None;
                        }
                    }
                }
                Step::RemoveToken { token } => {
                    if self.tokens.remove(token).is_none() {
                        return Err(format!("remove absent token {token}"));
                    }
                }
                Step::AddKeyslot { slot, by } => {
                    if self.keyslots.contains_key(slot) || !self.live(*by, key()?) {
                        return Err(format!("add {slot} by {by}"));
                    }
                    self.keyslots.insert(
                        *slot,
                        Slot {
                            key: new,
                            dead: false,
                        },
                    );
                }
                Step::ImportToken { token } => {
                    if !self.keyslots.contains_key(&token.keyslot()) {
                        return Err("token names an absent keyslot".into());
                    }
                    let free = (0..=MAX_SLOT)
                        .find(|n| !self.tokens.contains_key(n))
                        .ok_or("no free token")?;
                    self.tokens.insert(
                        free,
                        Entry {
                            slot: Some(token.keyslot()),
                            kind: Kind::Td {
                                role: token.role(),
                                chain: chain_of(token.sealed()),
                                key: key_of(token.sealed()),
                            },
                        },
                    );
                }
            }
            if self.td_tokens() > MAX_TD_TOKENS {
                return Err("over the reader's four-token bound".into());
            }
            Ok(())
        }
    }

    /// The machine one boot runs on: its boot chain, which a device-bound
    /// protector sealed to it releases on, and whether the first-boot
    /// protector releases (the TPM that sealed it, PCR 12 at zero).
    #[derive(Debug, Clone, Copy)]
    struct Machine {
        chain: u8,
        first_boot: bool,
    }

    const FIRST: Machine = Machine {
        chain: 1,
        first_boot: true,
    };

    /// Where a boot's plan stops.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Cut {
        /// After this many steps.
        After(usize),
        /// Inside this step, a kill, after it wiped the keyslot's area.
        Inside(usize),
    }

    const WHOLE: Cut = Cut::After(usize::MAX);

    /// What one boot did.
    #[derive(Debug, PartialEq, Eq)]
    enum Boot {
        /// The header admits no plan.
        Refused(Refusal),
        /// The plan's length, after any fall-back from a failed test.
        Planned(usize),
    }

    /// One boot as the selector will run it: release, classify, plan,
    /// confirm against cryptsetup's metadata and execute, falling back to
    /// the next released token when the first test fails and to a
    /// confirmed reseal when none is left. Keys come from what released,
    /// the recovery key on a reseal and the new protector's.
    fn boot(model: &mut Model, machine: Machine, cut: Cut, next_key: &mut u8) -> Boot {
        let header = model.header();
        if let Err(refusal) = check(&header) {
            return Boot::Refused(refusal);
        }
        let mut released: Vec<u8> = header
            .tokens
            .iter()
            .filter(|(_, token)| match token.role() {
                Role::FirstBoot => machine.first_boot,
                Role::DeviceBound => chain_of(token.sealed()) == machine.chain,
            })
            .map(|(number, _)| *number)
            .collect();
        loop {
            *next_key = next_key.wrapping_add(1).max(200);
            let new = *next_key;
            let transition = match classify(&header, &released).unwrap() {
                Released::DeviceBound { token } => Transition::Keep { token },
                Released::FirstBoot { token } => Transition::FirstBoot {
                    token,
                    sealed: sealed(machine.chain, new),
                },
                Released::Nothing => Transition::Reseal {
                    sealed: sealed(machine.chain, new),
                },
            };
            let plan = plan(&header, &transition).unwrap();
            let len = plan.steps().len();
            let steps = plan.confirm(&model.dump()).unwrap();
            let reseal = matches!(transition, Transition::Reseal { .. });
            let new_slot = steps.iter().find_map(|step| match step {
                Step::AddKeyslot { slot, .. } => Some(*slot),
                _ => None,
            });
            let key_of_slot = |slot: u8| -> Option<u8> {
                if slot == RECOVERY_SLOT {
                    return reseal.then_some(RECOVERY_KEY);
                }
                if Some(slot) == new_slot {
                    return Some(new);
                }
                header
                    .tokens
                    .iter()
                    .find(|(number, token)| released.contains(number) && token.keyslot() == slot)
                    .map(|(_, token)| key_of(token.sealed()))
            };
            let opener = match &transition {
                Transition::Keep { token } | Transition::FirstBoot { token, .. } => Some(*token),
                Transition::Reseal { .. } => None,
            };
            let mut failed = false;
            for (index, step) in steps.iter().enumerate() {
                if cut == Cut::After(index) {
                    return Boot::Planned(len);
                }
                let key = match step.input() {
                    Input::KeyOf(slot) => key_of_slot(slot),
                    _ => None,
                };
                let inside = cut == Cut::Inside(index);
                match model.apply(step, key, new, inside) {
                    Ok(()) if inside => return Boot::Planned(len),
                    Ok(()) => {}
                    Err(error) if index == 0 && opener.is_some() => {
                        // The released keyslot does not open: never halt.
                        assert!(matches!(step, Step::Test { .. }), "{error}");
                        failed = true;
                        break;
                    }
                    Err(error) => panic!("{error}: {step:?} of {steps:?} over {model:?}"),
                }
            }
            // The volume opens with the new key, the recovery key or the
            // released one; an empty plan tested nothing, so a dead
            // released keyslot is found here.
            let opens = match (new_slot, opener) {
                (Some(slot), _) => model.live(slot, new),
                (None, None) => true,
                (None, Some(token)) => header
                    .tokens
                    .iter()
                    .find(|(number, _)| *number == token)
                    .is_some_and(|(_, t)| model.live(t.keyslot(), key_of(t.sealed()))),
            };
            if !failed && opens {
                return Boot::Planned(len);
            }
            // Fall back without that token, to a reseal when none is left.
            let Some(token) = opener else {
                panic!("the recovery key does not open {model:?}")
            };
            released.retain(|number| *number != token);
        }
    }

    fn installed() -> Model {
        Model::new(&[0, 1], &[(0, td(1, Role::FirstBoot, 0))])
    }

    fn first_boot_done() -> Shape {
        shape(&[0, 2], &[(1, td(2, Role::DeviceBound, 1))])
    }

    /// The installed header's first boot, uninterrupted.
    #[test]
    fn the_first_boot_transition_replaces_the_first_boot_protector() {
        let header = installed().header();
        assert_eq!(
            classify(&header, &[0]).unwrap(),
            Released::FirstBoot { token: 0 }
        );
        let plan = plan(
            &header,
            &Transition::FirstBoot {
                token: 0,
                sealed: sealed(1, 200),
            },
        )
        .unwrap();
        let new = Token::new(2, Role::DeviceBound, sealed(1, 200)).unwrap();
        assert_eq!(
            plan.steps(),
            [
                Step::Test { slot: 1 },
                Step::AddKeyslot { slot: 2, by: 1 },
                Step::ImportToken { token: new },
                Step::Test { slot: 2 },
                Step::KillSlot { slot: 1, by: 2 },
                Step::RemoveToken { token: 0 },
            ]
        );
        let mut model = installed();
        let mut key = 0;
        assert_eq!(boot(&mut model, FIRST, WHOLE, &mut key), Boot::Planned(6));
        assert_eq!(model.shape(), first_boot_done());
        // The next boot on the same chain keeps it and changes nothing.
        assert_eq!(boot(&mut model, FIRST, WHOLE, &mut key), Boot::Planned(0));
    }

    /// Every cut of the first-boot transition, between steps or inside a
    /// kill, and every cut of the boot that resumes it, ends at the
    /// uninterrupted header on a later boot.
    #[test]
    fn every_interrupted_first_boot_transition_resumes_to_the_same_header() {
        let cuts = |len| {
            (0..=len)
                .map(Cut::After)
                .chain((0..len).map(Cut::Inside))
                .collect::<Vec<_>>()
        };
        for first in cuts(6) {
            for second in cuts(7) {
                let mut model = installed();
                let mut key = 0;
                let one = boot(&mut model, FIRST, first, &mut key);
                let before = model.clone();
                // A cut inside a step that is not a kill changes nothing.
                if matches!(first, Cut::Inside(_)) && model == installed() {
                    continue;
                }
                boot(&mut model, FIRST, second, &mut key);
                boot(&mut model, FIRST, WHOLE, &mut key);
                assert_eq!(
                    model.shape(),
                    first_boot_done(),
                    "cut {first:?} ({one:?}), then {second:?} from {before:?}"
                );
                assert!(model.keyslots.values().all(|s| !s.dead));
                assert_eq!(boot(&mut model, FIRST, WHOLE, &mut key), Boot::Planned(0));
            }
        }
    }

    /// The named interrupted states, and what the next boot's plan is.
    #[test]
    fn each_interrupted_state_has_its_resuming_plan() {
        let new = |slot| Token::new(slot, Role::DeviceBound, sealed(1, 200)).unwrap();
        // After the add: keyslot 2 is an orphan, the first-boot token
        // releases alone and the transition runs again over it.
        let after_add = Model::new(&[0, 1, 2], &[(0, td(1, Role::FirstBoot, 0))]);
        assert_eq!(
            plan(
                &after_add.header(),
                &Transition::FirstBoot {
                    token: 0,
                    sealed: sealed(1, 200)
                }
            )
            .unwrap()
            .steps(),
            [
                Step::Test { slot: 1 },
                Step::KillSlot { slot: 2, by: 1 },
                Step::AddKeyslot { slot: 2, by: 1 },
                Step::ImportToken { token: new(2) },
                Step::Test { slot: 2 },
                Step::KillSlot { slot: 1, by: 2 },
                Step::RemoveToken { token: 0 },
            ]
        );
        // After the import, and after the test: the new protector releases
        // and is tested before the first-boot one is retired.
        let after_import = Model::new(
            &[0, 1, 2],
            &[
                (0, td(1, Role::FirstBoot, 0)),
                (1, td(2, Role::DeviceBound, 1)),
            ],
        );
        let header = after_import.header();
        assert_eq!(
            classify(&header, &[0, 1]).unwrap(),
            Released::DeviceBound { token: 1 }
        );
        let keep = [
            Step::Test { slot: 2 },
            Step::KillSlot { slot: 1, by: 2 },
            Step::RemoveToken { token: 0 },
        ];
        assert_eq!(
            plan(&header, &Transition::Keep { token: 1 })
                .unwrap()
                .steps(),
            keep
        );
        // Inside the kill: keyslot 1 is dead, its JSON and token intact.
        // The header reads as after the test, and the kill runs again,
        // authorized by the new keyslot.
        let mut inside_kill = after_import.clone();
        if let Some(slot) = inside_kill.keyslots.get_mut(&1) {
            slot.dead = true;
        }
        assert_eq!(inside_kill.header(), header);
        // After the kill: the first-boot token is an orphan.
        let after_kill = Model::new(
            &[0, 2],
            &[
                (0, orphan(Role::FirstBoot)),
                (1, td(2, Role::DeviceBound, 1)),
            ],
        );
        let header = after_kill.header();
        assert_eq!(header.orphans, [(0, Role::FirstBoot)]);
        assert_eq!(
            plan(&header, &Transition::Keep { token: 1 })
                .unwrap()
                .steps(),
            [Step::Test { slot: 2 }, Step::RemoveToken { token: 0 }]
        );
        for mut model in [after_add, after_import, inside_kill, after_kill] {
            let mut key = 0;
            boot(&mut model, FIRST, WHOLE, &mut key);
            assert_eq!(model.shape(), first_boot_done());
        }
    }

    /// A dead released keyslot fails its test: the boot falls back to the
    /// next released token, and to a confirmed reseal when none is left,
    /// whose kill of the dead keyslot the recovery key authorizes.
    #[test]
    fn a_dead_released_keyslot_falls_back_and_never_halts() {
        let mut model = Model::new(
            &[0, 1, 2],
            &[
                (0, td(1, Role::DeviceBound, 1)),
                (1, td(2, Role::DeviceBound, 1)),
            ],
        );
        if let Some(slot) = model.keyslots.get_mut(&1) {
            slot.dead = true;
        }
        let mut key = 0;
        assert_eq!(boot(&mut model, FIRST, WHOLE, &mut key), Boot::Planned(3));
        assert_eq!(
            model.shape(),
            shape(&[0, 2], &[(1, td(2, Role::DeviceBound, 1))])
        );
        let mut model = installed();
        if let Some(slot) = model.keyslots.get_mut(&1) {
            slot.dead = true;
        }
        boot(&mut model, FIRST, WHOLE, &mut key);
        let (keyslots, tokens) = model.shape();
        assert_eq!(keyslots, [0, 2]);
        assert_eq!(tokens, [(1, Some(2), Some(Role::DeviceBound), 1)]);
    }

    /// A replaced TPM reaches recovery; the confirmed reseal retires every
    /// td protector but the new one, a surviving first-boot one included,
    /// and every interruption resumes to the same header.
    #[test]
    fn every_interrupted_reseal_resumes_to_the_same_header() {
        let start = || {
            Model::new(
                &[0, 1, 2],
                &[
                    (0, td(1, Role::FirstBoot, 0)),
                    (1, td(2, Role::DeviceBound, 1)),
                ],
            )
        };
        let changed = Machine {
            chain: 2,
            first_boot: false,
        };
        let header = start().header();
        assert_eq!(classify(&header, &[]).unwrap(), Released::Nothing);
        let new = Token::new(3, Role::DeviceBound, sealed(2, 200)).unwrap();
        assert_eq!(
            plan(
                &header,
                &Transition::Reseal {
                    sealed: sealed(2, 200)
                }
            )
            .unwrap()
            .steps(),
            [
                Step::AddKeyslot { slot: 3, by: 0 },
                Step::ImportToken { token: new },
                Step::Test { slot: 3 },
                Step::KillSlot { slot: 1, by: 3 },
                Step::KillSlot { slot: 2, by: 3 },
                Step::RemoveToken { token: 0 },
                Step::RemoveToken { token: 1 },
            ]
        );
        let done = shape(&[0, 3], &[(2, td(3, Role::DeviceBound, 2))]);
        let mut model = start();
        let mut key = 0;
        assert_eq!(boot(&mut model, changed, WHOLE, &mut key), Boot::Planned(7));
        assert_eq!(model.shape(), done);
        // Until the new token is committed nothing releases and the reseal
        // is confirmed again; once it is, it releases and is kept.
        let cuts: Vec<Cut> = (0..=7)
            .map(Cut::After)
            .chain((3..5).map(Cut::Inside))
            .collect();
        for first in &cuts {
            for second in &cuts {
                let mut model = start();
                boot(&mut model, changed, *first, &mut key);
                boot(&mut model, changed, *second, &mut key);
                boot(&mut model, changed, WHOLE, &mut key);
                assert_eq!(model.shape(), done, "cut {first:?}, then {second:?}");
            }
        }
    }

    /// The device-bound release keeps the lowest-numbered released token,
    /// tests it, and only then removes orphans, a leftover first-boot
    /// token and every superseded one.
    #[test]
    fn a_device_bound_release_retires_what_it_supersedes() {
        let cases: &[(Model, &[u8], Released, Vec<Step>)] = &[
            // Nothing to change: no step, not even the test.
            (
                Model::new(&[0, 2], &[(1, td(2, Role::DeviceBound, 1))]),
                &[1],
                Released::DeviceBound { token: 1 },
                vec![],
            ),
            // Two released: the lower number is kept.
            (
                Model::new(
                    &[0, 1, 2, 3],
                    &[
                        (0, td(1, Role::DeviceBound, 1)),
                        (1, td(2, Role::DeviceBound, 1)),
                        (2, td(3, Role::DeviceBound, 9)),
                    ],
                ),
                &[2, 1],
                Released::DeviceBound { token: 1 },
                vec![
                    Step::Test { slot: 2 },
                    Step::KillSlot { slot: 1, by: 2 },
                    Step::KillSlot { slot: 3, by: 2 },
                    Step::RemoveToken { token: 0 },
                    Step::RemoveToken { token: 2 },
                ],
            ),
            // An orphan keyslot, an orphan token and a foreign token's
            // keyslot, which is left alone.
            (
                Model::new(
                    &[0, 4, 5, 6],
                    &[
                        (0, orphan(Role::DeviceBound)),
                        (1, td(5, Role::DeviceBound, 1)),
                        (3, foreign(Some(6))),
                    ],
                ),
                &[1],
                Released::DeviceBound { token: 1 },
                vec![
                    Step::Test { slot: 5 },
                    Step::KillSlot { slot: 4, by: 5 },
                    Step::RemoveToken { token: 0 },
                ],
            ),
            // A first-boot token that also released is a leftover.
            (
                Model::new(
                    &[0, 1, 2],
                    &[
                        (0, td(1, Role::FirstBoot, 0)),
                        (3, td(2, Role::DeviceBound, 1)),
                    ],
                ),
                &[0, 3],
                Released::DeviceBound { token: 3 },
                vec![
                    Step::Test { slot: 2 },
                    Step::KillSlot { slot: 1, by: 2 },
                    Step::RemoveToken { token: 0 },
                ],
            ),
        ];
        for (index, (model, released, expected, steps)) in cases.iter().enumerate() {
            let header = model.header();
            let classified = classify(&header, released).unwrap();
            assert_eq!(classified, *expected, "case {index}");
            let Released::DeviceBound { token } = classified else {
                panic!("case {index}")
            };
            let plan = plan(&header, &Transition::Keep { token }).unwrap();
            assert_eq!(plan.steps(), *steps, "case {index}");
            assert_eq!(plan.is_empty(), steps.is_empty());
        }
    }

    /// When no device-bound token released nothing is superseded: the
    /// first-boot transition retires only first-boot tokens.
    #[test]
    fn a_first_boot_transition_leaves_device_bound_tokens() {
        let model = Model::new(
            &[0, 1, 3],
            &[
                (0, td(1, Role::FirstBoot, 0)),
                (1, td(3, Role::DeviceBound, 7)),
            ],
        );
        let header = model.header();
        assert_eq!(
            classify(&header, &[0]).unwrap(),
            Released::FirstBoot { token: 0 }
        );
        let new = Token::new(2, Role::DeviceBound, sealed(1, 200)).unwrap();
        assert_eq!(
            plan(
                &header,
                &Transition::FirstBoot {
                    token: 0,
                    sealed: sealed(1, 200)
                }
            )
            .unwrap()
            .steps(),
            [
                Step::Test { slot: 1 },
                Step::AddKeyslot { slot: 2, by: 1 },
                Step::ImportToken { token: new },
                Step::Test { slot: 2 },
                Step::KillSlot { slot: 1, by: 2 },
                Step::RemoveToken { token: 0 },
            ]
        );
    }

    fn four_tokens() -> Model {
        Model::new(
            &[0, 1, 2, 3, 4],
            &[
                (0, td(1, Role::FirstBoot, 0)),
                (1, td(2, Role::DeviceBound, 1)),
                (2, td(3, Role::DeviceBound, 2)),
                (3, td(4, Role::DeviceBound, 3)),
            ],
        )
    }

    /// At the four-token bound a plan first retires every td token that
    /// did not release, then adds: all but the released first-boot token,
    /// and in recovery every one.
    #[test]
    fn the_four_token_bound() {
        let header = four_tokens().header();
        let plan_of = |transition| plan(&header, &transition).unwrap().steps().to_vec();
        assert_eq!(
            plan_of(Transition::FirstBoot {
                token: 0,
                sealed: sealed(5, 200),
            }),
            [
                Step::Test { slot: 1 },
                Step::KillSlot { slot: 2, by: 1 },
                Step::KillSlot { slot: 3, by: 1 },
                Step::KillSlot { slot: 4, by: 1 },
                Step::RemoveToken { token: 1 },
                Step::RemoveToken { token: 2 },
                Step::RemoveToken { token: 3 },
                Step::AddKeyslot { slot: 2, by: 1 },
                Step::ImportToken {
                    token: Token::new(2, Role::DeviceBound, sealed(5, 200)).unwrap()
                },
                Step::Test { slot: 2 },
                Step::KillSlot { slot: 1, by: 2 },
                Step::RemoveToken { token: 0 },
            ]
        );
        let mut model = four_tokens();
        let mut key = 0;
        boot(
            &mut model,
            Machine {
                chain: 5,
                first_boot: true,
            },
            WHOLE,
            &mut key,
        );
        assert_eq!(
            model.shape(),
            shape(&[0, 2], &[(1, td(2, Role::DeviceBound, 5))])
        );
        let new = Token::new(1, Role::DeviceBound, sealed(5, 200)).unwrap();
        assert_eq!(
            plan_of(Transition::Reseal {
                sealed: sealed(5, 200)
            }),
            [
                Step::KillSlot { slot: 1, by: 0 },
                Step::KillSlot { slot: 2, by: 0 },
                Step::KillSlot { slot: 3, by: 0 },
                Step::KillSlot { slot: 4, by: 0 },
                Step::RemoveToken { token: 0 },
                Step::RemoveToken { token: 1 },
                Step::RemoveToken { token: 2 },
                Step::RemoveToken { token: 3 },
                Step::AddKeyslot { slot: 1, by: 0 },
                Step::ImportToken { token: new },
                Step::Test { slot: 1 },
            ]
        );
        // Orphans do not count once removed: three protectors and an
        // orphan token leave room.
        let mut model = four_tokens();
        model
            .apply(
                &Step::KillSlot { slot: 4, by: 0 },
                Some(RECOVERY_KEY),
                0,
                false,
            )
            .unwrap();
        let steps = plan(
            &model.header(),
            &Transition::FirstBoot {
                token: 0,
                sealed: sealed(5, 200),
            },
        )
        .unwrap();
        assert_eq!(steps.steps()[1], Step::RemoveToken { token: 3 });
        assert_eq!(steps.steps()[2], Step::AddKeyslot { slot: 4, by: 1 });
    }

    /// The import needs a free token number of any type: with every other
    /// number held by another token type the plan refuses before any
    /// step, and an orphan removed first makes room.
    #[test]
    fn a_plan_needs_a_free_token_number() {
        let mut tokens = vec![(0, td(1, Role::FirstBoot, 0))];
        tokens.extend((1..=MAX_SLOT).map(|n| (n, foreign(None))));
        let full = Model::new(&[0, 1], &tokens);
        let first_boot = Transition::FirstBoot {
            token: 0,
            sealed: sealed(1, 200),
        };
        assert_eq!(plan(&full.header(), &first_boot), Err(Refusal::NoFreeToken));
        assert_eq!(
            plan(
                &full.header(),
                &Transition::Reseal {
                    sealed: sealed(1, 200)
                }
            ),
            Err(Refusal::NoFreeToken)
        );
        // Keeping a protector imports nothing.
        let mut tokens = vec![(0, td(1, Role::DeviceBound, 1))];
        tokens.extend((1..=MAX_SLOT).map(|n| (n, foreign(None))));
        assert!(plan(
            &Model::new(&[0, 1], &tokens).header(),
            &Transition::Keep { token: 0 }
        )
        .unwrap()
        .is_empty());
        let mut tokens = vec![
            (0, td(1, Role::FirstBoot, 0)),
            (1, orphan(Role::DeviceBound)),
        ];
        tokens.extend((2..=MAX_SLOT).map(|n| (n, foreign(None))));
        let room = Model::new(&[0, 1], &tokens);
        let steps = plan(&room.header(), &first_boot).unwrap();
        assert_eq!(steps.steps()[1], Step::RemoveToken { token: 1 });
        let mut model = room;
        let mut key = 0;
        boot(&mut model, FIRST, WHOLE, &mut key);
        // The orphan's number went to the new token; the first-boot
        // token's is free again.
        assert_eq!(model.tokens.len(), 31);
    }

    /// The plan carries the view it was computed from: cryptsetup's own
    /// metadata must agree with it in keyslots and in tokens of every
    /// type, or no step is returned.
    #[test]
    fn a_plan_runs_only_on_metadata_that_agrees() {
        let model = Model::new(
            &[0, 1, 4],
            &[(0, td(1, Role::FirstBoot, 0)), (2, foreign(Some(4)))],
        );
        let header = model.header();
        let transition = Transition::FirstBoot {
            token: 0,
            sealed: sealed(1, 200),
        };
        let plan = plan(&header, &transition).unwrap();
        let steps = plan.steps().to_vec();
        assert_eq!(plan.clone().confirm(&model.dump()).unwrap(), steps);
        // A keyslot td saw as named by another token type is an orphan in
        // cryptsetup's copy, and the reverse.
        let mut disagreeing = Vec::new();
        let mut other = model.clone();
        other.tokens.remove(&2);
        disagreeing.push(other);
        let mut other = model.clone();
        other.keyslots.remove(&4);
        other.tokens.insert(2, foreign(None));
        disagreeing.push(other);
        // Another keyslot, another token of either kind, another seal.
        let mut other = model.clone();
        other.keyslots.insert(
            5,
            Slot {
                key: 105,
                dead: false,
            },
        );
        disagreeing.push(other);
        let mut other = model.clone();
        other.tokens.insert(3, foreign(None));
        disagreeing.push(other);
        let mut other = model.clone();
        other.tokens.insert(3, orphan(Role::DeviceBound));
        disagreeing.push(other);
        let mut other = model.clone();
        other.tokens.insert(0, td(1, Role::FirstBoot, 9));
        disagreeing.push(other);
        let mut other = model.clone();
        other.tokens.insert(0, td(1, Role::DeviceBound, 0));
        disagreeing.push(other);
        for (index, other) in disagreeing.iter().enumerate() {
            assert_eq!(
                plan.clone().confirm(&other.dump()),
                Err(Refusal::Metadata(
                    "cryptsetup's LUKS2 metadata disagrees with the header copy td read".into()
                )),
                "case {index}"
            );
        }
        // Metadata td cannot read refuses as well.
        let oversized = vec![b' '; luks2::MAX_METADATA_JSON + 1];
        for text in [&b""[..], b"[]", b"{\"keyslots\":{}}", &oversized] {
            assert!(matches!(
                plan.clone().confirm(text),
                Err(Refusal::Metadata(_))
            ));
        }
    }

    #[test]
    fn refusals() {
        let named_zero = Model::new(
            &[0, 1],
            &[
                (0, td(1, Role::FirstBoot, 0)),
                (2, td(0, Role::DeviceBound, 1)),
            ],
        );
        let shared = Model::new(
            &[0, 1],
            &[
                (0, td(1, Role::FirstBoot, 0)),
                (1, td(1, Role::DeviceBound, 1)),
            ],
        );
        let foreign_shared = Model::new(
            &[0, 1],
            &[(0, td(1, Role::FirstBoot, 0)), (5, foreign(Some(1)))],
        );
        let no_recovery = Model::new(&[1], &[(0, td(1, Role::FirstBoot, 0))]);
        let orphaned = Model::new(
            &[0, 2],
            &[
                (0, orphan(Role::FirstBoot)),
                (1, td(2, Role::DeviceBound, 1)),
            ],
        );
        // Every other keyslot named by a token of another type: no orphan
        // to make room.
        let full: Vec<u8> = (0..=MAX_SLOT).collect();
        let mut named = vec![(0, td(1, Role::FirstBoot, 0))];
        named.extend((2..=MAX_SLOT).map(|slot| (slot, foreign(Some(slot)))));
        let full = Model::new(&full, &named);
        let first_boot = |token| Transition::FirstBoot {
            token,
            sealed: sealed(1, 200),
        };
        let first_boot_zero = Model::new(&[0], &[(2, td(0, Role::FirstBoot, 1))]);
        let cases: Vec<(Model, Transition, Refusal)> = vec![
            (
                first_boot_zero,
                first_boot(2),
                Refusal::RecoveryKeyslotNamed { token: 2 },
            ),
            (
                named_zero.clone(),
                first_boot(0),
                Refusal::RecoveryKeyslotNamed { token: 2 },
            ),
            (
                named_zero.clone(),
                Transition::Reseal {
                    sealed: sealed(1, 200),
                },
                Refusal::RecoveryKeyslotNamed { token: 2 },
            ),
            (
                named_zero.clone(),
                Transition::Keep { token: 2 },
                Refusal::RecoveryKeyslotNamed { token: 2 },
            ),
            (shared, first_boot(0), Refusal::SharedKeyslot { slot: 1 }),
            (
                foreign_shared,
                first_boot(0),
                Refusal::SharedKeyslot { slot: 1 },
            ),
            (no_recovery, first_boot(0), Refusal::NoRecoveryKeyslot),
            (
                orphaned.clone(),
                Transition::Keep { token: 0 },
                Refusal::UnknownToken { token: 0 },
            ),
            (
                orphaned.clone(),
                first_boot(1),
                Refusal::WrongRole {
                    token: 1,
                    role: Role::DeviceBound,
                },
            ),
            (
                installed(),
                Transition::Keep { token: 0 },
                Refusal::WrongRole {
                    token: 0,
                    role: Role::FirstBoot,
                },
            ),
            (
                installed(),
                first_boot(4),
                Refusal::UnknownToken { token: 4 },
            ),
            (full, first_boot(0), Refusal::NoFreeKeyslot),
        ];
        for (index, (model, transition, refusal)) in cases.into_iter().enumerate() {
            let header = model.header();
            assert_eq!(
                plan(&header, &transition),
                Err(refusal.clone()),
                "case {index}"
            );
            assert!(!refusal.to_string().is_empty());
        }
        // Classification refuses an orphan's number and one naming
        // keyslot 0, which is never released; another td token beside it
        // still classifies: the volume opens, unplanned.
        assert_eq!(
            classify(&orphaned.header(), &[0]),
            Err(Refusal::UnknownToken { token: 0 })
        );
        assert_eq!(
            classify(&named_zero.header(), &[2]),
            Err(Refusal::RecoveryKeyslotNamed { token: 2 })
        );
        assert_eq!(
            classify(&named_zero.header(), &[0]),
            Ok(Released::FirstBoot { token: 0 })
        );
    }

    /// Each step's arguments and input are the runner's.
    #[test]
    fn each_step_is_one_runner_command() {
        let device = Path::new("/dev/sda2");
        let key = Path::new("/proc/7/fd/5");
        let token = Token::new(2, Role::DeviceBound, sealed(1, 200)).unwrap();
        let cases = [
            (
                Step::Test { slot: 2 },
                cryptsetup::test_args(device, 2),
                Input::KeyOf(2),
            ),
            (
                Step::KillSlot { slot: 1, by: 2 },
                cryptsetup::kill_slot_args(device, 1),
                Input::KeyOf(2),
            ),
            (
                Step::RemoveToken { token: 0 },
                cryptsetup::token_remove_args(device, 0),
                Input::Nothing,
            ),
            (
                Step::AddKeyslot { slot: 2, by: 1 },
                cryptsetup::add_key_args(device, 1, 2, key),
                Input::KeyOf(1),
            ),
            (
                Step::ImportToken {
                    token: token.clone(),
                },
                cryptsetup::token_import_args(device, None),
                Input::Token(&token),
            ),
        ];
        for (step, args, input) in cases {
            assert_eq!(step.args(device, key), args);
            assert_eq!(step.input(), input);
        }
    }

    /// A small deterministic generator for the probe below.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// A header of keyslots 0 to 5, some dead, and up to four tokens of
    /// every kind, foreign ones and orphans among them.
    fn random_header(rng: &mut Rng) -> Model {
        let keyslots: Vec<u8> = std::iter::once(0)
            .chain((1..=5).filter(|_| rng.below(2) == 0))
            .collect();
        let mut tokens = Vec::new();
        for _ in 0..rng.below(5) {
            let number = rng.below(6) as u8;
            if tokens.iter().any(|(n, _)| *n == number) {
                continue;
            }
            let slot = match rng.below(keyslots.len() as u64 + 1) as usize {
                0 => None,
                at => keyslots.get(at - 1).copied(),
            };
            let entry = match (rng.below(4), slot) {
                (0, _) => foreign(slot),
                (kind, Some(slot)) => {
                    let role = if kind == 1 {
                        Role::FirstBoot
                    } else {
                        Role::DeviceBound
                    };
                    td(slot, role, rng.below(3) as u8)
                }
                (kind, None) => orphan(if kind == 1 {
                    Role::FirstBoot
                } else {
                    Role::DeviceBound
                }),
            };
            tokens.push((number, entry));
        }
        let mut model = Model::new(&keyslots, &tokens);
        for (slot, state) in model.keyslots.iter_mut() {
            if *slot != RECOVERY_SLOT && rng.below(8) == 0 {
                state.dead = true;
            }
        }
        model
    }

    /// Over generated headers and three machines, every cut of the first
    /// boot, between steps or inside a kill, then further whole boots:
    /// no refusal but the header's own, no command cryptsetup would refuse
    /// (each test with the key that opens its keyslot, each kill and add
    /// with a live key), the reader's four-token bound after every step,
    /// and an empty plan within three boots.
    #[test]
    fn generated_headers_converge_through_every_cut() {
        let machines = [
            FIRST,
            Machine {
                chain: 2,
                first_boot: false,
            },
            Machine {
                chain: 1,
                first_boot: false,
            },
        ];
        let mut rng = Rng(0x7d_70_7e_c7_02);
        let mut runs = 0;
        for _ in 0..400 {
            let start = random_header(&mut rng);
            if check(&start.header()).is_err() {
                // The documented refusal: no plan, whatever the machine.
                let mut model = start.clone();
                let mut key = 0;
                assert!(matches!(
                    boot(&mut model, FIRST, WHOLE, &mut key),
                    Boot::Refused(_)
                ));
                assert_eq!(model, start);
                continue;
            }
            for machine in machines {
                let mut probe = start.clone();
                let mut key = 0;
                let Boot::Planned(len) = boot(&mut probe, machine, Cut::After(0), &mut key) else {
                    panic!("{start:?}")
                };
                let cuts = (0..=len).map(Cut::After).chain((0..len).map(Cut::Inside));
                for cut in cuts {
                    runs += 1;
                    let mut model = start.clone();
                    let mut key = 0;
                    boot(&mut model, machine, cut, &mut key);
                    let mut settled = false;
                    for _ in 0..3 {
                        if boot(&mut model, machine, WHOLE, &mut key) == Boot::Planned(0) {
                            settled = true;
                            break;
                        }
                    }
                    assert!(settled, "{start:?} on {machine:?} cut {cut:?}: {model:?}");
                    let header = model.header();
                    assert!(header.orphans.is_empty());
                    // Only a keyslot another token type names may stay
                    // dead: td leaves it alone.
                    let foreign = header.foreign_keyslots();
                    assert!(
                        model
                            .keyslots
                            .iter()
                            .all(|(slot, s)| !s.dead || foreign.contains(slot)),
                        "{start:?} on {machine:?} cut {cut:?}: {model:?}"
                    );
                }
            }
        }
        assert!(runs > 1000, "{runs}");
    }
}
