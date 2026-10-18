use super::*;
use crate::luks2::HeaderCopy;
use crate::pin::PinPcrs;
use crate::protected_plan;
use crate::token::{self as device, Role};
use std::collections::BTreeMap;
use td_tpm::SealedObject;

/// The recovery key's identity in the model.
pub(crate) const RECOVERY_KEY: u32 = 1000;
/// A pending fido2 token `n`'s passphrase is `FIDO_KEY + n`.
pub(crate) const FIDO_KEY: u32 = 2000;
/// The device-bound protector's secret.
const DEVICE_KEY: u32 = 101;
/// The salt text every modelled digest carries.
const SALT: &str = "c2FsdA==";

/// A keyslot as cryptsetup keeps it: the key it opens with, the volume key
/// it wraps, and whether it is a `luks2` keyslot or a reencryption's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) key: u32,
    pub(crate) vk: u32,
    pub(crate) luks2: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tok {
    Protected(Token),
    /// A first-boot or device-bound td token, an orphan naming none.
    Bound {
        slot: Option<u8>,
        role: Role,
    },
    Foreign(Vec<u8>),
}

/// A LUKS2 header as cryptsetup keeps it, with the data segment's volume
/// key generation and an online reencryption's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Disk {
    pub(crate) keyslots: BTreeMap<u8, Slot>,
    pub(crate) tokens: BTreeMap<u8, Tok>,
    pub(crate) vk: u32,
    next_vk: u32,
    pub(crate) reencrypting: Option<u32>,
    /// The `mode` a reencryption keyslot carries.
    mode: &'static str,
}

/// Where a power cut stops a reencryption among cryptsetup's commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Partial {
    /// After its new key's keyslot, before the initialization's commit:
    /// an unbound keyslot no token names.
    Unbound,
    /// After its initialization: U2.
    Initialized,
    /// After the data, between the commits that destroy the old key's
    /// keyslots one at a time and the one that drops the requirement.
    Finishing,
}

fn segment(vk: u32) -> [u8; 32] {
    luks2::segment_digest(&format!("vk{vk}"), SALT)
}

/// A tpm-pin protector releasing `key`.
fn pin(slot: Option<u8>, key: u32) -> TpmPin {
    TpmPin {
        keyslot: slot,
        pcrs: PinPcrs::Selector,
        primary: [0x0b; 34],
        salt: [0x5a; 32],
        sealed: SealedObject {
            public: key.to_be_bytes().to_vec(),
            private: vec![1],
        },
    }
}

fn key_of(pin: &TpmPin) -> u32 {
    u32::from_be_bytes(pin.sealed.public[..4].try_into().unwrap())
}

fn fido(slot: Option<u8>, role: FidoRole, n: u8) -> Token {
    Token::Fido2(Fido2 {
        keyslot: slot,
        role,
        credential: vec![n; 16],
        salt: [3; 32],
        x: [4; 32],
        y: [5; 32],
    })
}

fn lockout_token() -> Lockout {
    Lockout {
        primary: [0x0b; 34],
        nonce: [1; 32],
        ciphertext: [2; 32],
        tag: [3; 32],
        proven: false,
    }
}

fn bound_token(slot: u8, role: Role) -> device::Token {
    device::Token::new(
        slot,
        role,
        SealedObject {
            public: vec![0x00, 0x08, slot],
            private: vec![0x01, slot],
        },
    )
    .unwrap()
}

/// The secret `key` stands for this boot, `tpm` the sealed one's.
fn secret(key: Key, tpm: Option<u32>) -> u32 {
    match key {
        Key::Recovery => RECOVERY_KEY,
        Key::Fido2 { token } => FIDO_KEY + u32::from(token),
        Key::TpmPin => tpm.unwrap(),
    }
}

impl Disk {
    /// U0 as phase 1 leaves it: the recovery key in keyslot 0, the
    /// device-bound protector in keyslot 1 as token 0, the pending fido2
    /// tokens 1 and 2 (in the FIDO2 shape the first is primary) and the
    /// `td-upgrade` token 3 over volume key 1.
    pub(crate) fn u0(shape: Shape) -> Self {
        let mut disk = Self {
            keyslots: BTreeMap::from([
                (
                    0,
                    Slot {
                        key: RECOVERY_KEY,
                        vk: 1,
                        luks2: true,
                    },
                ),
                (
                    1,
                    Slot {
                        key: DEVICE_KEY,
                        vk: 1,
                        luks2: true,
                    },
                ),
            ]),
            tokens: BTreeMap::new(),
            vk: 1,
            next_vk: 2,
            reencrypting: None,
            mode: "reencrypt",
        };
        disk.tokens.insert(
            0,
            Tok::Bound {
                slot: Some(1),
                role: Role::DeviceBound,
            },
        );
        let first = match shape {
            Shape::Tpm => FidoRole::Recovery,
            Shape::Fido2 => FidoRole::Primary,
        };
        disk.tokens.insert(1, Tok::Protected(fido(None, first, 1)));
        disk.tokens
            .insert(2, Tok::Protected(fido(None, FidoRole::Recovery, 2)));
        disk.tokens.insert(
            3,
            Tok::Protected(Token::Upgrade {
                shape,
                from: segment(1),
            }),
        );
        disk
    }

    pub(crate) fn header(&self) -> Header {
        let mut header = Header {
            copy: HeaderCopy::Primary,
            seqid: 1,
            hdr_size: 0x4000,
            uuid: "0f0e0d0c-0b0a-4908-8706-050403020100".into(),
            label: "td-system".into(),
            keyslots: self.keyslots.keys().copied().collect(),
            tokens: Vec::new(),
            orphans: Vec::new(),
            foreign: Vec::new(),
            protected: Vec::new(),
        };
        for (number, token) in &self.tokens {
            match token {
                Tok::Protected(token) => header.protected.push((*number, token.clone())),
                Tok::Bound {
                    slot: Some(slot),
                    role,
                } => header.tokens.push((*number, bound_token(*slot, *role))),
                Tok::Bound { slot: None, role } => header.orphans.push((*number, *role)),
                Tok::Foreign(slots) => header.foreign.push((*number, slots.clone())),
            }
        }
        header
    }

    fn volume(&self) -> Volume {
        Volume {
            segment: Some(segment(self.vk)),
            reencryption: self.reencrypting.is_some(),
            luks2: self
                .keyslots
                .iter()
                .filter(|(_, slot)| slot.luks2)
                .map(|(number, _)| *number)
                .collect(),
            reencrypt_modes: self
                .keyslots
                .values()
                .filter(|slot| !slot.luks2)
                .map(|_| self.mode.to_owned())
                .collect(),
        }
    }

    fn token_json(token: &Tok) -> String {
        match token {
            Tok::Protected(token) => token.encode().unwrap(),
            Tok::Bound {
                slot: Some(slot),
                role,
            } => bound_token(*slot, *role).encode(),
            Tok::Bound { slot: None, role } => format!(
                r#"{{"type":"td-protector","keyslots":[],"role":"{}","public":"0008","private":"01"}}"#,
                role.name()
            ),
            Tok::Foreign(slots) => {
                let slots: Vec<String> = slots.iter().map(|s| format!("\"{s}\"")).collect();
                format!(
                    r#"{{"type":"systemd-tpm2","keyslots":[{}]}}"#,
                    slots.join(",")
                )
            }
        }
    }

    /// The compact text cryptsetup would write, by the measured sizes.
    fn json_len(&self) -> usize {
        let luks2 = self.keyslots.values().filter(|slot| slot.luks2).count();
        let reencryption = if self.reencrypting.is_some() { 903 } else { 0 };
        let tokens: usize = self
            .tokens
            .values()
            .map(|token| Self::token_json(token).len() + 6)
            .sum();
        427 + 305 * luks2 + reencryption + tokens
    }

    pub(crate) fn view(&self) -> UpgradeView {
        UpgradeView {
            header: self.header(),
            volume: self.volume(),
            json_len: self.json_len(),
        }
    }

    /// The header as `luksDump --dump-json-metadata` prints it, which the
    /// reader's own bounds then judge.
    pub(crate) fn dump(&self) -> Vec<u8> {
        let keyslots: Vec<String> = self
            .keyslots
            .iter()
            .map(|(number, slot)| {
                if slot.luks2 {
                    format!("\"{number}\": {{\"type\": \"luks2\"}}")
                } else {
                    format!(
                        "\"{number}\": {{\"type\": \"reencrypt\", \"mode\": \"{}\"}}",
                        self.mode
                    )
                }
            })
            .collect();
        let tokens: Vec<String> = self
            .tokens
            .iter()
            .map(|(number, token)| format!("\"{number}\": {}", Self::token_json(token)))
            .collect();
        let wrapping = |vk: u32| -> String {
            let slots: Vec<String> = self
                .keyslots
                .iter()
                .filter(|(_, slot)| slot.vk == vk)
                .map(|(number, _)| format!("\"{number}\""))
                .collect();
            slots.join(", ")
        };
        let mut keys: Vec<u32> = vec![self.vk];
        keys.extend(self.keyslots.values().map(|slot| slot.vk));
        keys.retain(|vk| *vk != 0);
        keys.sort_unstable();
        keys.dedup();
        let digests: Vec<String> = keys
            .iter()
            .enumerate()
            .map(|(number, vk)| {
                let segments = if *vk == self.vk {
                    "\"0\""
                } else if Some(*vk) == self.reencrypting {
                    "\"2\""
                } else {
                    ""
                };
                format!(
                    "\"{number}\": {{\"type\": \"pbkdf2\", \"keyslots\": [{}], \"segments\": [{segments}], \"digest\": \"vk{vk}\", \"salt\": \"{SALT}\"}}",
                    wrapping(*vk)
                )
            })
            .collect();
        let digests = digests.join(", ");
        let mut config = String::from("\"json_size\": \"12288\"");
        if self.reencrypting.is_some() {
            config.push_str(", \"requirements\": {\"mandatory\": [\"online-reencrypt-v2\"]}");
        }
        format!(
            "{{\"keyslots\": {{{}}}, \"tokens\": {{{}}}, \"segments\": {{\"0\": {{\"type\": \"crypt\"}}}}, \"digests\": {{{digests}}}, \"config\": {{{config}}}}}",
            keyslots.join(", "),
            tokens.join(", ")
        )
        .into_bytes()
    }

    /// Whether `slot` opens the data segment with `key`.
    pub(crate) fn opens(&self, slot: u8, key: u32) -> bool {
        self.keyslots
            .get(&slot)
            .is_some_and(|s| s.luks2 && s.key == key && s.vk == self.vk)
    }

    fn free_slot(&self) -> u8 {
        (0..=MAX_SLOT)
            .find(|slot| !self.keyslots.contains_key(slot))
            .unwrap()
    }

    /// cryptsetup strips a destroyed keyslot from every token naming it.
    fn strip(&mut self, slot: u8) {
        for token in self.tokens.values_mut() {
            match token {
                Tok::Protected(
                    Token::TpmPin(TpmPin { keyslot, .. })
                    | Token::Fido2(Fido2 { keyslot, .. })
                    | Token::RecoveryKey { keyslot },
                ) if *keyslot == Some(slot) => *keyslot = None,
                Tok::Bound { slot: named, .. } if *named == Some(slot) => *named = None,
                Tok::Foreign(slots) => slots.retain(|s| *s != slot),
                _ => {}
            }
        }
    }

    /// A reencryption's end: every keyslot of another volume key, its own
    /// keyslot among them, wiped.
    fn finish(&mut self) {
        let new = self.reencrypting.take().unwrap();
        let old: Vec<u8> = self
            .keyslots
            .iter()
            .filter(|(_, slot)| slot.vk != new)
            .map(|(number, _)| *number)
            .collect();
        for slot in old {
            self.keyslots.remove(&slot);
            self.strip(slot);
        }
        self.vk = new;
    }

    /// Destroys the old key's keyslots, one commit each in cryptsetup.
    fn wipe_old(&mut self) {
        let old: Vec<u8> = self
            .keyslots
            .iter()
            .filter(|(_, slot)| slot.luks2 && slot.vk == self.vk)
            .map(|(number, _)| *number)
            .collect();
        for slot in old {
            self.keyslots.remove(&slot);
            self.strip(slot);
        }
    }

    /// One step as cryptsetup commits it, `tpm` the sealed secret this
    /// boot. `partial` stops a reencryption where a power cut would.
    pub(crate) fn apply(&mut self, step: &Step, tpm: Option<u32>, partial: Option<Partial>) {
        match step {
            Step::Test { slot, key } => {
                assert!(self.opens(*slot, secret(*key, tpm)), "{step:?}")
            }
            Step::KillSlot { slot, by } => {
                assert!(slot != by && self.opens(*by, RECOVERY_KEY), "{step:?}");
                assert!(self.keyslots.remove(slot).is_some(), "{step:?}");
                self.strip(*slot);
            }
            Step::RemoveToken { token } => {
                assert!(self.tokens.remove(token).is_some(), "{step:?}")
            }
            Step::AddKeyslot { slot, by, key } => {
                assert!(self.opens(*by, RECOVERY_KEY), "{step:?}");
                let new = Slot {
                    key: secret(*key, tpm),
                    vk: self.vk,
                    luks2: true,
                };
                assert!(self.keyslots.insert(*slot, new).is_none(), "{step:?}");
            }
            Step::ImportToken { token, id, replace } => {
                if let Some(slot) = token.keyslot() {
                    assert!(self.keyslots.contains_key(&slot), "{step:?}");
                }
                let number = match id {
                    Some(id) => {
                        assert_eq!(self.tokens.contains_key(id), *replace, "{step:?}");
                        *id
                    }
                    None => (0..=MAX_SLOT)
                        .find(|n| !self.tokens.contains_key(n))
                        .unwrap(),
                };
                self.tokens.insert(number, Tok::Protected(token.clone()));
            }
            Step::Reencrypt { slot, .. } => {
                assert!(self.reencrypting.is_none(), "{step:?}");
                assert!(self.opens(*slot, RECOVERY_KEY), "{step:?}");
                // cryptsetup moves a token naming the unlocked keyslot onto
                // the rewrapped one (`assign_tokens`); `check` admits none.
                assert!(
                    self.tokens.values().all(|token| match token {
                        Tok::Protected(token) => token.keyslot() != Some(*slot),
                        Tok::Bound { slot: named, .. } => *named != Some(*slot),
                        Tok::Foreign(slots) => !slots.contains(slot),
                    }),
                    "{step:?}"
                );
                let new = self.next_vk;
                self.next_vk += 1;
                let rewrapped = self.free_slot();
                self.keyslots.insert(
                    rewrapped,
                    Slot {
                        key: RECOVERY_KEY,
                        vk: new,
                        luks2: true,
                    },
                );
                if partial != Some(Partial::Unbound) {
                    self.reencrypting = Some(new);
                    let own = self.free_slot();
                    self.keyslots.insert(
                        own,
                        Slot {
                            key: 0,
                            vk: 0,
                            luks2: false,
                        },
                    );
                    match partial {
                        None => self.finish(),
                        Some(Partial::Finishing) => self.wipe_old(),
                        Some(_) => {}
                    }
                }
            }
            Step::ResumeReencrypt => {
                // cryptsetup opens a keyslot of each volume key.
                let new = self.reencrypting.unwrap();
                for vk in [self.vk, new] {
                    assert!(
                        self.keyslots
                            .values()
                            .any(|s| s.luks2 && s.key == RECOVERY_KEY && s.vk == vk),
                        "{step:?}"
                    );
                }
                match partial {
                    None => self.finish(),
                    Some(Partial::Finishing) => self.wipe_old(),
                    Some(_) => {}
                }
            }
            Step::TakeLockout => {
                let (_, import) = lockout_import(&self.view(), &lockout_token()).unwrap();
                self.apply(&import, tpm, None);
            }
        }
        // The reader admits every state a step leaves, as cryptsetup's
        // metadata.
        let (metadata, volume) = luks2::upgrade_metadata(&self.dump()).unwrap();
        assert_eq!(metadata, self.header().metadata());
        assert_eq!(volume, self.volume());
    }
}

/// Where a boot stops: after `steps` steps, inside the next one at
/// `inside` when it is a reencryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cut {
    steps: usize,
    inside: Option<Partial>,
}

/// The cuts inside a reencryption the upgrade converges from: between
/// steps, before the initialization's commit, and after it.
const INSIDE: &[Option<Partial>] = &[None, Some(Partial::Unbound), Some(Partial::Initialized)];

/// One boot of the upgrade: the gather, then each state's plan, confirmed,
/// until the `td-upgrade` token is gone or `cut` stops it. A usable TPM
/// seals a fresh secret whenever one is needed. It returns the steps run.
fn boot(
    disk: &mut Disk,
    cut: Option<Cut>,
    usable: bool,
    resilience: Resilience,
    fresh: &mut u32,
) -> usize {
    let view = disk.view();
    if view.header.kind() != Kind::Upgrading {
        return 0;
    }
    let needs = needs(&view).unwrap();
    *fresh += 1;
    let sealed = (needs.tpm_pin && usable).then(|| pin(None, 3000 + *fresh));
    let tpm = match (&sealed, usable) {
        (_, false) => Tpm::Unusable,
        (Some(sealed), true) => Tpm::Sealed(sealed),
        (None, true) => Tpm::None,
    };
    let tpm_key = sealed.as_ref().map(key_of);
    let gathered = Gathered {
        fido2: &needs.fido2,
        dropped: &[],
        tpm,
        resilience,
    };
    let mut ran = 0;
    for _ in 0..8 {
        let view = disk.view();
        if view.header.kind() != Kind::Upgrading {
            return ran;
        }
        let steps = plan(&view, &gathered)
            .unwrap()
            .confirm(&disk.dump())
            .unwrap();
        assert!(!steps.is_empty());
        for step in &steps {
            // A U5 boot released as a protected one: no step needs the
            // recovery key.
            if !needs.recovery_key {
                assert_ne!(step.input(), Input::Key(Key::Recovery), "{step:?}");
            }
            if let Some(cut) = cut.filter(|cut| cut.steps == ran) {
                let reencrypts = matches!(step, Step::Reencrypt { .. } | Step::ResumeReencrypt);
                if let Some(partial) = cut.inside.filter(|_| reencrypts) {
                    disk.apply(step, tpm_key, Some(partial));
                }
                return ran;
            }
            disk.apply(step, tpm_key, None);
            ran += 1;
        }
    }
    panic!("the upgrade boot did not end: {disk:?}");
}

/// The upgrade's one final header: the shape's protectors and the recovery
/// key's marker, no `td-upgrade`, device-bound or orphan token, no keyslot
/// that no token names, every protector opening the rotated volume key,
/// and `tpm` tpm-pin and lockout tokens.
fn assert_final(disk: &Disk, shape: Shape, tpm: (usize, usize), case: &str) {
    let header = disk.header();
    assert_eq!(header.kind(), Kind::Protected, "{case}: {disk:?}");
    assert_eq!(protected_plan::check(&header), Ok(shape), "{case}");
    assert!(
        header.tokens.is_empty() && header.orphans.is_empty(),
        "{case}"
    );
    assert_ne!(disk.vk, 1, "{case}: the volume key did not rotate");
    let mut named = Vec::new();
    let mut pins = 0;
    let mut lockouts = 0;
    for (number, token) in &header.protected {
        match token {
            Token::RecoveryKey { keyslot } => {
                let slot = keyslot.unwrap();
                assert!(disk.opens(slot, RECOVERY_KEY), "{case}");
                named.push(slot);
            }
            Token::Fido2(fido) => {
                let slot = fido.keyslot.unwrap();
                assert!(disk.opens(slot, FIDO_KEY + u32::from(*number)), "{case}");
                named.push(slot);
            }
            Token::TpmPin(pin) => {
                let slot = pin.keyslot.unwrap();
                assert!(disk.opens(slot, key_of(pin)), "{case}");
                named.push(slot);
                pins += 1;
            }
            Token::Lockout(_) => lockouts += 1,
            other => panic!("{case}: {other:?} remains"),
        }
    }
    named.sort_unstable();
    assert_eq!(
        disk.keyslots.keys().copied().collect::<Vec<_>>(),
        named,
        "{case}"
    );
    let fidos = header
        .protected
        .iter()
        .filter(|(_, t)| matches!(t, Token::Fido2(_)))
        .count();
    assert_eq!(fidos, 2, "{case}");
    assert_eq!((pins, lockouts), tpm, "{case}: {disk:?}");
}

/// Both of the TPM shape's tpm-pin and lockout tokens, none in the other.
fn full(shape: Shape) -> (usize, usize) {
    match shape {
        Shape::Tpm => (1, 1),
        Shape::Fido2 => (0, 0),
    }
}

const RESILIENCES: &[Resilience] = &[Resilience::Checksum, Resilience::Journal];
const SHAPES: &[Shape] = &[Shape::Tpm, Shape::Fido2];

/// Boots after a first cut until the upgrade ends, the second cut too.
fn converge(mut disk: Disk, cuts: [Option<Cut>; 2], usable: bool, resilience: Resilience) -> Disk {
    let mut fresh = 0;
    for cut in cuts.into_iter().chain([None, None]) {
        boot(&mut disk, cut, usable, resilience, &mut fresh);
    }
    assert_ne!(disk.header().kind(), Kind::Upgrading, "{disk:?}");
    disk
}

/// Every cut of the upgrade boot, between steps and inside the
/// reencryption, and every cut of the boot that resumes it, in both
/// shapes and resilience modes: the boots after converge on one final
/// header.
#[test]
fn every_cut_converges_on_one_protected_header() {
    for &shape in SHAPES {
        for &resilience in RESILIENCES {
            let mut whole = Disk::u0(shape);
            let total = boot(&mut whole, None, true, resilience, &mut 0);
            assert_final(&whole, shape, full(shape), "uncut");
            for first in 0..=total {
                for &inside in INSIDE {
                    let cut = Cut {
                        steps: first,
                        inside,
                    };
                    let mut after = Disk::u0(shape);
                    boot(&mut after, Some(cut), true, resilience, &mut 0);
                    let rest = boot(&mut after.clone(), None, true, resilience, &mut 0);
                    for second in 0..=rest {
                        for &inside in INSIDE {
                            let second = Cut {
                                steps: second,
                                inside,
                            };
                            let done = converge(
                                Disk::u0(shape),
                                [Some(cut), Some(second)],
                                true,
                                resilience,
                            );
                            let case = format!("{shape:?} {resilience:?} {cut:?} {second:?}");
                            assert_final(&done, shape, full(shape), &case);
                        }
                    }
                }
            }
        }
    }
}

/// The states a cut leaves follow ENCRYPTION.md's table in order, each of
/// U0 to U5 reached by some cut, and each asks before the cap only what it
/// still needs.
#[test]
fn the_states_follow_the_table() {
    let mut seen = Vec::new();
    let mut whole = Disk::u0(Shape::Tpm);
    let total = boot(&mut whole, None, true, Resilience::Journal, &mut 0);
    for steps in 0..=total {
        for &inside in INSIDE {
            let mut disk = Disk::u0(Shape::Tpm);
            boot(
                &mut disk,
                Some(Cut { steps, inside }),
                true,
                Resilience::Journal,
                &mut 0,
            );
            let view = disk.view();
            if view.header.kind() != Kind::Upgrading {
                continue;
            }
            let needs = needs(&view).unwrap();
            if let Some(last) = seen.last() {
                assert!(needs.state >= *last, "{needs:?} after {last:?}");
            }
            seen.push(needs.state);
            let before = needs.state != State::U5;
            assert_eq!(needs.recovery_key, before, "{needs:?}");
            assert_eq!(needs.state.cancellable(), needs.state == State::U0);
            assert_eq!(needs.state.needs_mains(), needs.state == State::U0);
            let pending: Vec<u8> = super::pending(&view.header);
            assert_eq!(needs.fido2, if before { pending } else { vec![] });
        }
    }
    seen.dedup();
    assert_eq!(
        seen,
        [
            State::U0,
            State::U1,
            State::U2,
            State::U3,
            State::U4,
            State::U5
        ]
    );
}

/// U5, which a cut before the last commit leaves, gathers nothing: the
/// boot releases with the PIN as a protected one, and its plan removes the
/// `td-upgrade` token without the recovery key.
#[test]
fn u5_completes_without_the_recovery_key() {
    let mut whole = Disk::u0(Shape::Tpm);
    let total = boot(&mut whole, None, true, Resilience::Journal, &mut 0);
    let mut disk = Disk::u0(Shape::Tpm);
    boot(
        &mut disk,
        Some(Cut {
            steps: total - 1,
            inside: None,
        }),
        true,
        Resilience::Journal,
        &mut 0,
    );
    let view = disk.view();
    let needs = needs(&view).unwrap();
    assert_eq!(
        needs,
        Needs {
            state: State::U5,
            shape: Shape::Tpm,
            fido2: vec![],
            tpm_pin: false,
            lockout: false,
            recovery_key: false,
        }
    );
    let gathered = Gathered {
        fido2: &[],
        dropped: &[],
        tpm: Tpm::None,
        resilience: Resilience::Journal,
    };
    assert_eq!(
        plan(&view, &gathered).unwrap().steps(),
        [Step::RemoveToken { token: 3 }]
    );
    boot(&mut disk, None, true, Resilience::Journal, &mut 0);
    assert_final(&disk, Shape::Tpm, (1, 1), "U5");
}

/// A TPM unusable for td from the first boot, or from any cut on: the
/// upgrade still completes, as a TPM-primary volume without a tpm-pin or
/// lockout token, or with whatever an earlier boot already committed.
#[test]
fn a_tpm_that_becomes_unusable_ends_the_upgrade_without_a_tpm_pin() {
    let mut disk = Disk::u0(Shape::Tpm);
    let mut fresh = 0;
    boot(&mut disk, None, false, Resilience::Journal, &mut fresh);
    assert_final(&disk, Shape::Tpm, (0, 0), "unusable from the start");
    let mut whole = Disk::u0(Shape::Tpm);
    let total = boot(&mut whole, None, true, Resilience::Journal, &mut 0);
    for (steps, &inside) in (0..=total).flat_map(|steps| INSIDE.iter().map(move |i| (steps, i))) {
        let mut disk = Disk::u0(Shape::Tpm);
        let mut fresh = 0;
        let cut = Cut { steps, inside };
        boot(&mut disk, Some(cut), true, Resilience::Journal, &mut fresh);
        boot(&mut disk, None, false, Resilience::Journal, &mut fresh);
        let header = disk.header();
        assert_eq!(header.kind(), Kind::Protected, "{cut:?}");
        let pin = protected_plan::tpm_pins(&header).len();
        let lockout = protected_plan::lockout(&header).is_some();
        // The tpm-pin protector and the take commit in that order, and a
        // boot without the TPM adds neither.
        assert!(!lockout || pin == 1, "{cut:?}: {disk:?}");
        assert_final(
            &disk,
            Shape::Tpm,
            (pin, usize::from(lockout)),
            &format!("{cut:?}"),
        );
    }
}

/// A missing mains supply holds the upgrade at U0, and only there.
#[test]
fn the_upgrade_waits_for_mains_power() {
    assert!(on_mains(&[]));
    assert!(on_mains(&[("Mains", true), ("Battery", true)]));
    assert!(on_mains(&[("USB", false), ("Mains", true)]));
    assert!(!on_mains(&[("Mains", false), ("Battery", true)]));
    assert!(!on_mains(&[("Battery", false)]));
    assert!(on_mains(&[("UPS", true)]));
    let needs = needs(&Disk::u0(Shape::Tpm).view()).unwrap();
    assert!(needs.state.needs_mains());
}

#[test]
fn the_resilience_follows_the_disk_atomic_write_unit() {
    assert_eq!(resilience(None), Resilience::Journal);
    assert_eq!(resilience(Some(0)), Resilience::Journal);
    assert_eq!(resilience(Some(4095)), Resilience::Journal);
    assert_eq!(resilience(Some(4096)), Resilience::Checksum);
    assert_eq!(resilience(Some(65536)), Resilience::Checksum);
}

/// A cancellation in U0 removes `td-upgrade` and the pending tokens, at
/// every cut: what a cut leaves is a device-bound header whose pending
/// tokens `pending_orphans` removes. After U0 nothing cancels.
#[test]
fn a_cancelled_upgrade_leaves_a_device_bound_header_without_pending_tokens() {
    let disk = Disk::u0(Shape::Fido2);
    let plan = cancel(&disk.view()).unwrap();
    assert_eq!(
        plan.steps(),
        [
            Step::RemoveToken { token: 3 },
            Step::RemoveToken { token: 1 },
            Step::RemoveToken { token: 2 },
        ]
    );
    let steps = plan.confirm(&disk.dump()).unwrap();
    for cut in 0..=steps.len() {
        let mut cancelled = disk.clone();
        for step in &steps[..cut] {
            cancelled.apply(step, None, None);
        }
        let header = cancelled.header();
        if header.kind() == Kind::Upgrading {
            continue;
        }
        let orphans = pending_orphans(&header).unwrap();
        for step in orphans.confirm(&cancelled.dump()).unwrap() {
            cancelled.apply(&step, None, None);
        }
        let header = cancelled.header();
        assert_eq!(header.kind(), Kind::DeviceBound);
        assert!(header.protected.is_empty(), "{cut}: {header:?}");
        assert_eq!(header.keyslots, [0, 1]);
        assert_eq!(header.tokens.len(), 1);
    }
    let mut u1 = Disk::u0(Shape::Tpm);
    boot(
        &mut u1,
        Some(Cut {
            steps: 4,
            inside: None,
        }),
        true,
        Resilience::Journal,
        &mut 0,
    );
    assert_eq!(check(&u1.view()).unwrap().0, State::U1);
    assert_eq!(cancel(&u1.view()), Err(Refusal::NotCancellable));
    let u2 = in_state(Shape::Tpm, State::U2);
    assert_eq!(cancel(&u2.view()), Err(Refusal::NotCancellable));
    // A U0 header `check` refuses, here for a foreign token that joined it
    // after phase 1, still cancels: its removals lose nothing.
    let mut foreign = Disk::u0(Shape::Tpm);
    foreign.tokens.insert(9, Tok::Foreign(vec![0]));
    assert_eq!(check(&foreign.view()), Err(Refusal::Foreign { token: 9 }));
    let steps = cancel(&foreign.view())
        .unwrap()
        .confirm(&foreign.dump())
        .unwrap();
    for step in &steps {
        foreign.apply(step, None, None);
    }
    let header = foreign.header();
    assert_eq!(header.kind(), Kind::DeviceBound);
    assert!(header.protected.is_empty());
    assert_eq!(header.foreign.len(), 1);
    // A U0 header holding a protected token that is neither pending nor
    // `td-upgrade` does not cancel: without `td-upgrade` the reader would
    // refuse its mix of device-bound and protected tokens.
    for extra in [
        Token::RecoveryKey { keyslot: Some(0) },
        fido(Some(1), FidoRole::Recovery, 7),
        Token::TpmPin(pin(None, 3001)),
    ] {
        let mut mixed = Disk::u0(Shape::Tpm);
        mixed.tokens.insert(9, Tok::Protected(extra.clone()));
        assert_eq!(
            cancel(&mixed.view()),
            Err(Refusal::NotCancellable),
            "{extra:?}"
        );
    }
    // A header that is not upgrading cancels nothing.
    let mut device_bound = Disk::u0(Shape::Tpm);
    device_bound.tokens.remove(&3);
    assert_eq!(cancel(&device_bound.view()), Err(Refusal::NotUpgrading));
}

/// Phase 1 interrupted before its `td-upgrade` token leaves pending tokens
/// on a device-bound header: they are removed, and nothing else.
#[test]
fn an_interrupted_phase_1_leaves_no_pending_tokens() {
    let mut disk = Disk::u0(Shape::Tpm);
    disk.tokens.remove(&3);
    let plan = pending_orphans(&disk.header()).unwrap();
    assert_eq!(
        plan.steps(),
        [
            Step::RemoveToken { token: 1 },
            Step::RemoveToken { token: 2 }
        ]
    );
    for step in plan.confirm(&disk.dump()).unwrap() {
        disk.apply(&step, None, None);
    }
    assert!(disk.header().protected.is_empty());
    assert_eq!(disk.keyslots.len(), 2);
    assert_eq!(
        pending_orphans(&Disk::u0(Shape::Tpm).header()),
        Err(Refusal::NotDeviceBound)
    );
}

/// The disk after the uncut upgrade boot's first `steps` steps.
fn cut_at(shape: Shape, steps: usize, inside: Option<Partial>) -> Disk {
    let mut disk = Disk::u0(shape);
    boot(
        &mut disk,
        Some(Cut { steps, inside }),
        true,
        Resilience::Journal,
        &mut 0,
    );
    disk
}

/// The disk at the first cut in `state`, a cut inside the reencryption
/// among them.
fn in_state(shape: Shape, state: State) -> Disk {
    (0..64)
        .flat_map(|steps| {
            INSIDE
                .iter()
                .map(move |inside| cut_at(shape, steps, *inside))
        })
        .find(|disk| check(&disk.view()).is_ok_and(|(found, _)| found == state))
        .unwrap()
}

/// Forged and malformed upgrading headers: each refused before any step.
#[test]
fn forged_headers_plan_nothing() {
    let mut cases: Vec<(&str, Disk, Refusal)> = Vec::new();
    let mut disk = Disk::u0(Shape::Tpm);
    disk.tokens.insert(9, Tok::Foreign(vec![0]));
    cases.push(("a foreign token", disk, Refusal::Foreign { token: 9 }));
    let mut disk = Disk::u0(Shape::Tpm);
    disk.tokens
        .insert(1, Tok::Protected(fido(None, FidoRole::Primary, 1)));
    cases.push((
        "a fido2-primary token in the TPM shape",
        disk,
        Refusal::MixedShape,
    ));
    let mut disk = Disk::u0(Shape::Fido2);
    disk.tokens
        .insert(1, Tok::Protected(fido(None, FidoRole::Recovery, 1)));
    cases.push((
        "the FIDO2 shape without a primary",
        disk,
        Refusal::NoPrimary,
    ));
    let mut disk = in_state(Shape::Fido2, State::U4);
    disk.tokens
        .insert(8, Tok::Protected(Token::Lockout(lockout_token())));
    cases.push((
        "a lockout token in the FIDO2 shape",
        disk,
        Refusal::MixedShape,
    ));
    let mut disk = Disk::u0(Shape::Tpm);
    disk.tokens.insert(
        8,
        Tok::Protected(Token::Request {
            operation: crate::protected::Operation::PinChange,
        }),
    );
    cases.push(("a request", disk, Refusal::Unexpected { token: 8 }));
    let mut disk = Disk::u0(Shape::Tpm);
    disk.tokens
        .insert(8, Tok::Protected(Token::TpmPin(pin(Some(0), 7))));
    cases.push((
        "a tpm-pin token before the marker",
        disk,
        Refusal::Unexpected { token: 8 },
    ));
    let mut disk = Disk::u0(Shape::Tpm);
    disk.tokens
        .insert(8, Tok::Protected(Token::RecoveryKey { keyslot: Some(0) }));
    cases.push((
        "a marker before the reencryption",
        disk,
        Refusal::Unexpected { token: 8 },
    ));
    let mut disk = in_state(Shape::Tpm, State::U2);
    disk.tokens
        .insert(8, Tok::Protected(Token::RecoveryKey { keyslot: Some(0) }));
    cases.push((
        "a marker inside the reencryption",
        disk,
        Refusal::Unexpected { token: 8 },
    ));
    let mut disk = in_state(Shape::Tpm, State::U3);
    disk.keyslots.insert(
        5,
        Slot {
            key: DEVICE_KEY,
            vk: disk.vk,
            luks2: true,
        },
    );
    disk.tokens.insert(
        8,
        Tok::Bound {
            slot: Some(5),
            role: Role::DeviceBound,
        },
    );
    cases.push((
        "a device-bound token after the reencryption",
        disk,
        Refusal::Unexpected { token: 8 },
    ));
    let mut disk = Disk::u0(Shape::Tpm);
    disk.keyslots.insert(
        6,
        Slot {
            key: 0,
            vk: 0,
            luks2: false,
        },
    );
    cases.push((
        "a reencryption keyslot outside a reencryption",
        disk,
        Refusal::NotLuks2 { slot: 6 },
    ));
    let mut disk = Disk::u0(Shape::Tpm);
    disk.tokens.insert(
        0,
        Tok::Bound {
            slot: Some(0),
            role: Role::DeviceBound,
        },
    );
    cases.push((
        "a device-bound token naming keyslot 0",
        disk,
        Refusal::Unexpected { token: 0 },
    ));
    let mut disk = Disk::u0(Shape::Tpm);
    disk.keyslots.remove(&0);
    cases.push(("no keyslot 0", disk, Refusal::NoRecoveryKeyslot));
    let mut disk = in_state(Shape::Tpm, State::U4);
    let (marker, _) = disk
        .tokens
        .iter()
        .find(|(_, t)| matches!(t, Tok::Protected(Token::RecoveryKey { .. })))
        .map(|(n, t)| (*n, t.clone()))
        .unwrap();
    disk.tokens
        .insert(marker, Tok::Protected(Token::RecoveryKey { keyslot: None }));
    cases.push((
        "a marker naming no keyslot",
        disk,
        Refusal::NoRecoveryKeyslot,
    ));
    let mut disk = in_state(Shape::Tpm, State::U4);
    let marked = disk
        .tokens
        .values()
        .find_map(|t| match t {
            Tok::Protected(Token::RecoveryKey { keyslot }) => *keyslot,
            _ => None,
        })
        .unwrap();
    disk.tokens
        .insert(1, Tok::Protected(fido(Some(marked), FidoRole::Recovery, 1)));
    cases.push((
        "a fido2 token on the recovery keyslot",
        disk,
        Refusal::SharedKeyslot { slot: marked },
    ));
    let mut disk = in_state(Shape::Tpm, State::U4);
    disk.tokens
        .insert(8, Tok::Protected(Token::TpmPin(pin(None, 7))));
    disk.tokens
        .insert(9, Tok::Protected(Token::TpmPin(pin(None, 8))));
    cases.push(("two tpm-pin tokens", disk, Refusal::Unexpected { token: 9 }));
    let gathered = Gathered {
        fido2: &[1, 2],
        dropped: &[],
        tpm: Tpm::Unusable,
        resilience: Resilience::Journal,
    };
    for (case, disk, refusal) in cases {
        let view = disk.view();
        assert_eq!(check(&view), Err(refusal.clone()), "{case}");
        assert_eq!(needs(&view), Err(refusal.clone()), "{case}");
        assert_eq!(plan(&view, &gathered), Err(refusal.clone()), "{case}");
        assert_eq!(
            lockout_import(&view, &lockout_token()),
            Err(refusal),
            "{case}"
        );
    }
    // Two keyslots no token names in U3: a header the state admits, but no
    // plan can tell the recovery keyslot.
    let mut disk = in_state(Shape::Tpm, State::U3);
    let vk = disk.vk;
    disk.keyslots.insert(
        5,
        Slot {
            key: 7,
            vk,
            luks2: true,
        },
    );
    assert_eq!(
        plan(&disk.view(), &gathered),
        Err(Refusal::NoRecoveryKeyslot)
    );
    // No digest naming the data segment.
    let mut view = Disk::u0(Shape::Tpm).view();
    view.volume.segment = None;
    assert_eq!(check(&view), Err(Refusal::NoSegment));
    // A device-bound and a protected header are not upgrading ones.
    let mut device_bound = Disk::u0(Shape::Tpm);
    device_bound.tokens.remove(&3);
    assert_eq!(check(&device_bound.view()), Err(Refusal::NotUpgrading));
    let mut protected = in_state(Shape::Fido2, State::U5);
    protected.tokens.remove(&3);
    assert_eq!(check(&protected.view()), Err(Refusal::NotUpgrading));
}

/// A plan whose header could outgrow the JSON area is refused before its
/// first step: the reencryption's growth in U1, a keyslot and its token's
/// in U4.
#[test]
fn a_plan_that_would_not_fit_is_refused() {
    let gathered = Gathered {
        fido2: &[1, 2],
        dropped: &[],
        tpm: Tpm::Unusable,
        resilience: Resilience::Checksum,
    };
    let mut view = in_state(Shape::Tpm, State::U1).view();
    view.json_len = JSON_AREA_LEN - REENCRYPT_JSON - 1;
    assert!(plan(&view, &gathered).is_ok());
    view.json_len += 1;
    assert_eq!(
        plan(&view, &gathered),
        Err(Refusal::TooLarge {
            bytes: JSON_AREA_LEN
        })
    );
    let mut view = in_state(Shape::Tpm, State::U4).view();
    view.json_len = JSON_AREA_LEN - 2 * KEYSLOT_JSON;
    assert!(matches!(
        plan(&view, &gathered),
        Err(Refusal::TooLarge { .. })
    ));
    // A removal alone always fits a header that fits.
    let mut view = in_state(Shape::Fido2, State::U5).view();
    view.json_len = JSON_AREA_LEN - 1;
    assert!(plan(&view, &gathered).is_ok());
}

/// What a state needs and was not gathered is refused before any step.
#[test]
fn a_secret_not_gathered_is_refused() {
    let view = in_state(Shape::Tpm, State::U4).view();
    let mut gathered = Gathered {
        fido2: &[1],
        dropped: &[],
        tpm: Tpm::Unusable,
        resilience: Resilience::Journal,
    };
    assert_eq!(
        plan(&view, &gathered),
        Err(Refusal::MissingFido2 { token: 2 })
    );
    gathered.fido2 = &[1, 2];
    gathered.tpm = Tpm::None;
    assert_eq!(plan(&view, &gathered), Err(Refusal::NoTpmPin));
    // The FIDO2 shape seals nothing.
    let view = in_state(Shape::Fido2, State::U4).view();
    assert!(plan(&view, &gathered).is_ok());
}

/// The U4 plan, written out: the marker's keyslot, where the reencryption
/// rewrapped the recovery key, tested, an orphan an earlier add left
/// killed, each pending token's keyslot added and tested before its token
/// is replaced in place to name it, then the tpm-pin protector and the
/// take.
#[test]
fn the_u4_plan_adds_each_protector_under_the_recovery_key() {
    let mut disk = in_state(Shape::Tpm, State::U4);
    let marked = disk
        .tokens
        .values()
        .find_map(|t| match t {
            Tok::Protected(Token::RecoveryKey { keyslot }) => *keyslot,
            _ => None,
        })
        .unwrap();
    assert_eq!(marked, 1);
    let vk = disk.vk;
    disk.keyslots.insert(
        5,
        Slot {
            key: FIDO_KEY + 1,
            vk,
            luks2: true,
        },
    );
    let sealed = pin(None, 3001);
    let gathered = Gathered {
        fido2: &[1, 2],
        dropped: &[],
        tpm: Tpm::Sealed(&sealed),
        resilience: Resilience::Journal,
    };
    let plan = plan(&disk.view(), &gathered).unwrap();
    let replaced = |n: u8, slot: u8| Step::ImportToken {
        token: fido(Some(slot), FidoRole::Recovery, n),
        id: Some(n),
        replace: true,
    };
    assert_eq!(
        plan.steps(),
        [
            Step::Test {
                slot: 1,
                key: Key::Recovery
            },
            Step::KillSlot { slot: 5, by: 1 },
            Step::AddKeyslot {
                slot: 2,
                by: 1,
                key: Key::Fido2 { token: 1 }
            },
            Step::Test {
                slot: 2,
                key: Key::Fido2 { token: 1 }
            },
            replaced(1, 2),
            Step::AddKeyslot {
                slot: 3,
                by: 1,
                key: Key::Fido2 { token: 2 }
            },
            Step::Test {
                slot: 3,
                key: Key::Fido2 { token: 2 }
            },
            replaced(2, 3),
            Step::AddKeyslot {
                slot: 4,
                by: 1,
                key: Key::TpmPin
            },
            Step::Test {
                slot: 4,
                key: Key::TpmPin
            },
            Step::ImportToken {
                token: Token::TpmPin(pin(Some(4), 3001)),
                id: None,
                replace: false
            },
            Step::TakeLockout,
        ]
    );
    let device = Path::new("/proc/1/fd/5");
    let new_key = Path::new("/proc/1/fd/6");
    let args: Vec<Option<Vec<OsString>>> = plan
        .steps()
        .iter()
        .map(|step| step.args(device, "td-selector", new_key))
        .collect();
    assert_eq!(
        args[2],
        Some(cryptsetup::add_key_args(device, 1, 2, new_key))
    );
    assert_eq!(args[4], Some(cryptsetup::token_replace_args(device, 1)));
    assert_eq!(args[11], None);
    assert_eq!(plan.steps()[2].new_key(), Some(Key::Fido2 { token: 1 }));
    assert_eq!(plan.steps()[2].input(), Input::Key(Key::Recovery));
    assert_eq!(plan.steps()[3].input(), Input::Key(Key::Fido2 { token: 1 }));
}

/// The U0, U1 and U2 plans, written out, and their reencryption's
/// arguments.
#[test]
fn the_reencryption_runs_with_the_recovery_key_on_keyslot_0() {
    let gathered = Gathered {
        fido2: &[1, 2],
        dropped: &[],
        tpm: Tpm::Unusable,
        resilience: Resilience::Checksum,
    };
    // An orphan keyslot beside the device-bound one: the reader admits no
    // second device-bound token beside `td-upgrade`.
    let mut disk = Disk::u0(Shape::Tpm);
    disk.keyslots.insert(
        2,
        Slot {
            key: 7,
            vk: 1,
            luks2: true,
        },
    );
    let u0 = plan(&disk.view(), &gathered).unwrap();
    assert_eq!(
        u0.steps(),
        [
            Step::Test {
                slot: 0,
                key: Key::Recovery
            },
            Step::KillSlot { slot: 1, by: 0 },
            Step::KillSlot { slot: 2, by: 0 },
            Step::RemoveToken { token: 0 },
        ]
    );
    for step in u0.confirm(&disk.dump()).unwrap() {
        disk.apply(&step, None, None);
    }
    let u1 = plan(&disk.view(), &gathered).unwrap();
    assert_eq!(
        u1.steps(),
        [
            Step::Test {
                slot: 0,
                key: Key::Recovery
            },
            Step::Reencrypt {
                slot: 0,
                resilience: Resilience::Checksum
            },
        ]
    );
    let device = Path::new("/proc/1/fd/5");
    assert_eq!(
        u1.steps()[1].args(device, "td-selector", Path::new("/unused")),
        Some(cryptsetup::reencrypt_args(
            device,
            "td-selector",
            0,
            Resilience::Checksum
        ))
    );
    disk.apply(&u1.steps()[1], None, Some(Partial::Initialized));
    let u2 = plan(&disk.view(), &gathered).unwrap();
    assert_eq!(u2.steps(), [Step::ResumeReencrypt]);
    assert_eq!(
        u2.steps()[0].args(device, "td-selector", Path::new("/unused")),
        Some(cryptsetup::reencrypt_resume_args(device, "td-selector"))
    );
    disk.apply(&u2.steps()[0], None, None);
    // The rewrapped recovery keyslot is the one no token names.
    let u3 = plan(&disk.view(), &gathered).unwrap();
    assert_eq!(
        u3.steps(),
        [
            Step::Test {
                slot: 1,
                key: Key::Recovery
            },
            Step::ImportToken {
                token: Token::RecoveryKey { keyslot: Some(1) },
                id: None,
                replace: false
            },
        ]
    );
}

/// A plan runs only over metadata that agrees with its view, the data
/// segment and the requirement included.
#[test]
fn metadata_that_disagrees_runs_no_step() {
    let gathered = Gathered {
        fido2: &[1, 2],
        dropped: &[],
        tpm: Tpm::Unusable,
        resilience: Resilience::Journal,
    };
    let disk = in_state(Shape::Tpm, State::U1);
    let plan = plan(&disk.view(), &gathered).unwrap();
    let mut rotated = disk.clone();
    rotated.vk = 9;
    let mut reencrypting = disk.clone();
    reencrypting.reencrypting = Some(9);
    for other in [rotated, reencrypting] {
        assert!(matches!(
            plan.clone().confirm(&other.dump()),
            Err(Refusal::Metadata(_))
        ));
    }
    assert!(matches!(plan.confirm(b"{"), Err(Refusal::Metadata(_))));
}

/// The take's lockout token goes to the lowest free number, and only a U4
/// TPM-shape header with a tpm-pin keyslot and no lockout token takes one.
#[test]
fn the_lockout_import_belongs_to_u4_in_the_tpm_shape() {
    let mut whole = Disk::u0(Shape::Tpm);
    let total = boot(&mut whole, None, true, Resilience::Journal, &mut 0);
    let before_take = cut_at(Shape::Tpm, total - 2, None);
    let view = before_take.view();
    assert_eq!(check(&view).unwrap().0, State::U4);
    let (id, step) = lockout_import(&view, &lockout_token()).unwrap();
    // The marker took number 0, which U0's commit freed, and the tpm-pin
    // token 4.
    assert_eq!(id, 5);
    assert_eq!(
        step,
        Step::ImportToken {
            token: Token::Lockout(lockout_token()),
            id: Some(5),
            replace: false
        }
    );
    for state in [State::U0, State::U3, State::U5] {
        let view = in_state(Shape::Tpm, state).view();
        assert_eq!(
            lockout_import(&view, &lockout_token()),
            Err(Refusal::NoTake),
            "{state:?}"
        );
    }
    // In U4 before the tpm-pin protector's keyslot.
    let view = in_state(Shape::Tpm, State::U4).view();
    assert_eq!(
        lockout_import(&view, &lockout_token()),
        Err(Refusal::NoTake)
    );
}

/// A cut after the reencryption's new-key keyslot and before its
/// initialization leaves U1 with an unbound keyslot no token names: U1's
/// plan kills it before it starts again.
#[test]
fn an_unbound_keyslot_a_cut_initialization_left_is_killed() {
    let mut whole = Disk::u0(Shape::Tpm);
    let total = boot(&mut whole, None, true, Resilience::Journal, &mut 0);
    let disk = (0..total)
        .map(|steps| cut_at(Shape::Tpm, steps, Some(Partial::Unbound)))
        .find(|disk| {
            disk.keyslots.len() == 2
                && check(&disk.view()).is_ok_and(|(state, _)| state == State::U1)
        })
        .unwrap();
    let gathered = Gathered {
        fido2: &[1, 2],
        dropped: &[],
        tpm: Tpm::Unusable,
        resilience: Resilience::Journal,
    };
    assert_eq!(check(&disk.view()).unwrap().0, State::U1);
    assert_eq!(
        plan(&disk.view(), &gathered).unwrap().steps(),
        [
            Step::Test {
                slot: 0,
                key: Key::Recovery
            },
            Step::KillSlot { slot: 1, by: 0 },
            Step::Reencrypt {
                slot: 0,
                resilience: Resilience::Journal
            },
        ]
    );
}

/// cryptsetup destroys the old key's keyslots one commit at a time before
/// the commit that drops its requirement (ENCRYPTION.md "Threat and
/// limits"). A cut there leaves U2 with no keyslot for the old key, which
/// no resumption can open: the planner still names U2 and offers nothing
/// but the resumption, so no boot writes anything over it.
#[test]
fn a_cut_inside_cryptsetups_last_commits_plans_only_the_resumption() {
    let disk = in_state(Shape::Tpm, State::U1);
    let mut finishing = disk.clone();
    let gathered = Gathered {
        fido2: &[1, 2],
        dropped: &[],
        tpm: Tpm::Unusable,
        resilience: Resilience::Journal,
    };
    let u1 = plan(&disk.view(), &gathered).unwrap();
    let reencrypt = u1
        .steps()
        .iter()
        .find(|step| matches!(step, Step::Reencrypt { .. }))
        .unwrap();
    for step in u1.steps() {
        let partial = (step == reencrypt).then_some(Partial::Finishing);
        finishing.apply(step, None, partial);
    }
    let view = finishing.view();
    assert!(view.volume.reencryption);
    assert!(finishing
        .keyslots
        .values()
        .all(|slot| slot.vk != finishing.vk));
    assert_eq!(check(&view).unwrap().0, State::U2);
    assert_eq!(
        plan(&view, &gathered).unwrap().steps(),
        [Step::ResumeReencrypt]
    );
}

/// A reencryption keyslot in another mode than `reencrypt` is refused:
/// resuming a forged `decrypt` would leave the volume in clear.
#[test]
fn a_reencryption_in_another_mode_is_refused() {
    let mut disk = in_state(Shape::Tpm, State::U2);
    for mode in ["decrypt", "encrypt", ""] {
        disk.mode = mode;
        assert_eq!(check(&disk.view()), Err(Refusal::NotReencryption), "{mode}");
    }
}

/// A TPM found unusable after the tpm-pin protector's commit: `needs` asks
/// the selector to read the TPM for the take, and passed as unusable the
/// plan ends with the `td-upgrade` token's removal instead of a take that
/// would fail on every boot.
#[test]
fn a_take_on_an_unusable_tpm_is_not_planned() {
    let mut whole = Disk::u0(Shape::Tpm);
    let total = boot(&mut whole, None, true, Resilience::Journal, &mut 0);
    let disk = cut_at(Shape::Tpm, total - 2, None);
    let view = disk.view();
    let needs = needs(&view).unwrap();
    assert_eq!(
        (needs.state, needs.tpm_pin, needs.lockout),
        (State::U4, false, true)
    );
    let usable = Gathered {
        fido2: &[],
        dropped: &[],
        tpm: Tpm::None,
        resilience: Resilience::Journal,
    };
    let marker = super::marker(&view.header).unwrap().unwrap();
    assert_eq!(
        plan(&view, &usable).unwrap().steps(),
        [
            Step::Test {
                slot: marker,
                key: Key::Recovery
            },
            Step::TakeLockout
        ]
    );
    let unusable = Gathered {
        tpm: Tpm::Unusable,
        ..usable
    };
    assert_eq!(
        plan(&view, &unusable).unwrap().steps(),
        [
            Step::Test {
                slot: marker,
                key: Key::Recovery
            },
            Step::RemoveToken { token: 3 }
        ]
    );
    // The FIDO2 shape takes nothing.
    let fido = super::needs(&in_state(Shape::Fido2, State::U4).view()).unwrap();
    assert!(!fido.tpm_pin && !fido.lockout);
}

/// Every state's plan run to the end under `gathered`, uncut.
fn run_all(disk: &mut Disk, gathered: &Gathered, tpm: Option<u32>) {
    for _ in 0..8 {
        let view = disk.view();
        if view.header.kind() != Kind::Upgrading {
            return;
        }
        let steps = plan(&view, gathered)
            .unwrap()
            .confirm(&disk.dump())
            .unwrap();
        for step in &steps {
            disk.apply(step, tpm, None);
        }
    }
    panic!("the upgrade did not end: {disk:?}");
}

/// A dropped pending key: its token goes in U4 and no keyslot is added for
/// it, in either shape, but the FIDO2 shape keeps a fido2-primary token,
/// and only a pending token not yet dropped may go.
#[test]
fn a_dropped_key_is_removed_and_the_fido2_shapes_last_refused() {
    for &shape in SHAPES {
        let view = Disk::u0(shape).view();
        assert!(droppable(&view, &[], 2), "{shape:?}");
        assert!(!droppable(&view, &[2], 2), "{shape:?}");
        assert!(!droppable(&view, &[], 3), "{shape:?}: td-upgrade");
        assert!(!droppable(&view, &[], 7), "{shape:?}: no token");
        // Token 1 is the FIDO2 shape's one fido2-primary token.
        assert_eq!(droppable(&view, &[], 1), shape == Shape::Tpm, "{shape:?}");
        assert_eq!(droppable(&view, &[2], 1), shape == Shape::Tpm, "{shape:?}");
        let sealed = pin(None, 3001);
        let mut disk = Disk::u0(shape);
        let gathered = Gathered {
            fido2: &[1],
            dropped: &[2],
            tpm: match shape {
                Shape::Tpm => Tpm::Sealed(&sealed),
                Shape::Fido2 => Tpm::None,
            },
            resilience: Resilience::Journal,
        };
        run_all(&mut disk, &gathered, Some(key_of(&sealed)));
        let header = disk.header();
        assert_eq!(
            protected_plan::check(&header),
            Ok(shape),
            "{shape:?} {disk:?}"
        );
        // A later import may take the dropped token's number.
        let fidos = header
            .protected
            .iter()
            .filter(|(_, token)| matches!(token, Token::Fido2(_)))
            .count();
        assert_eq!(fidos, 1, "{shape:?}");
        assert!(
            header
                .protected
                .iter()
                .any(|(n, t)| *n == 1 && matches!(t, Token::Fido2(fido) if fido.keyslot.is_some())),
            "{shape:?}"
        );
        // The FIDO2 shape's last fido2-primary token, and a token that is
        // not pending, refuse the plan.
        let view = in_state(shape, State::U4).view();
        let dropping = |dropped: &'static [u8]| Gathered {
            fido2: &[1, 2],
            dropped,
            tpm: Tpm::Unusable,
            resilience: Resilience::Journal,
        };
        if shape == Shape::Fido2 {
            assert_eq!(plan(&view, &dropping(&[1])), Err(Refusal::NoPrimary));
        }
        assert_eq!(
            plan(&view, &dropping(&[3])),
            Err(Refusal::Unexpected { token: 3 }),
            "{shape:?}"
        );
    }
}

/// A primary whose Name changed between the seal and the take: U4 kills
/// the new tpm-pin keyslot and removes its token, then the `td-upgrade`
/// token, so no keyslot remains for an object no lockout token anchors.
#[test]
fn a_changed_primary_removes_the_new_tpm_pin_protector() {
    let mut whole = Disk::u0(Shape::Tpm);
    let total = boot(&mut whole, None, true, Resilience::Journal, &mut 0);
    let mut disk = cut_at(Shape::Tpm, total - 2, None);
    let view = disk.view();
    let (number, slot) = view
        .header
        .protected
        .iter()
        .find_map(|(number, token)| match token {
            Token::TpmPin(pin) => Some((*number, pin.keyslot.unwrap())),
            _ => None,
        })
        .unwrap();
    let marker = super::marker(&view.header).unwrap().unwrap();
    let changed = Gathered {
        fido2: &[],
        dropped: &[],
        tpm: Tpm::Changed,
        resilience: Resilience::Journal,
    };
    assert_eq!(
        plan(&view, &changed).unwrap().steps(),
        [
            Step::Test {
                slot: marker,
                key: Key::Recovery
            },
            Step::KillSlot { slot, by: marker },
            Step::RemoveToken { token: number },
            Step::RemoveToken { token: 3 }
        ]
    );
    run_all(&mut disk, &changed, None);
    let header = disk.header();
    assert_eq!(protected_plan::check(&header), Ok(Shape::Tpm));
    assert!(!header
        .protected
        .iter()
        .any(|(_, token)| matches!(token, Token::TpmPin(_) | Token::Lockout(_))));
    assert!(!disk.keyslots.contains_key(&slot));
}

/// U4 whose every token number is taken refuses the tpm-pin import before
/// its first step. (Every keyslot from 1 taken cannot be reached in U4: a
/// keyslot no token names is an orphan its plan frees, and protected
/// tokens name one keyslot each.)
#[test]
fn a_u4_with_no_free_token_number_is_refused() {
    let sealed = pin(None, 3001);
    let gathered = Gathered {
        fido2: &[1, 2],
        dropped: &[],
        tpm: Tpm::Sealed(&sealed),
        resilience: Resilience::Journal,
    };
    let mut disk = in_state(Shape::Tpm, State::U4);
    assert!(plan(&disk.view(), &gathered).is_ok());
    // Past the reader's one-lockout bound, to reach the planner's own count.
    for number in 0..=31u8 {
        disk.tokens
            .entry(number)
            .or_insert(Tok::Protected(Token::Lockout(lockout_token())));
    }
    assert_eq!(plan(&disk.view(), &gathered), Err(Refusal::NoFreeToken));
}

/// A converged device-bound header, as an install's first boot leaves it:
/// the recovery key in keyslot 0 and the device-bound protector in
/// keyslot 1 as token 0.
fn converged() -> Disk {
    let mut disk = Disk::u0(Shape::Tpm);
    disk.tokens.retain(|number, _| *number == 0);
    disk
}

/// Phase 1's new key `n` in `shape`, at its largest when `largest`.
fn new_key(shape: Shape, n: u8, largest: bool) -> Fido2 {
    Fido2 {
        keyslot: None,
        role: match shape {
            Shape::Tpm => FidoRole::Recovery,
            Shape::Fido2 => FidoRole::Primary,
        },
        credential: vec![n; if largest { MAX_CREDENTIAL_ID } else { 16 }],
        salt: [n; 32],
        x: [n; 32],
        y: [n; 32],
    }
}

/// Phase 1 over a converged header imports each new key's pending token and
/// last `td-upgrade` naming the shape and the data segment's identity; over
/// a crashed phase 1's header it removes the orphans first. Every cut
/// leaves a device-bound header, whose pending tokens are orphans, or U0;
/// and the upgrade boot then converges on the shape's protected header.
#[test]
fn phase_one_takes_a_converged_header_to_u0() {
    for &shape in SHAPES {
        for orphans in [false, true] {
            let mut disk = converged();
            if orphans {
                disk.tokens
                    .insert(1, Tok::Protected(Token::Fido2(new_key(shape, 8, false))));
                disk.tokens
                    .insert(2, Tok::Protected(Token::Fido2(new_key(shape, 9, false))));
            }
            let keys = [new_key(shape, 1, false), new_key(shape, 2, false)];
            let plan = phase_one(&disk.view(), shape, &keys).unwrap();
            let mut expected = Vec::new();
            if orphans {
                expected.extend([
                    Step::RemoveToken { token: 1 },
                    Step::RemoveToken { token: 2 },
                ]);
            }
            expected.extend(
                keys.iter()
                    .map(|key| Token::Fido2(key.clone()))
                    .chain([Token::Upgrade {
                        shape,
                        from: segment(1),
                    }])
                    .map(|token| Step::ImportToken {
                        token,
                        id: None,
                        replace: false,
                    }),
            );
            assert_eq!(plan.steps(), expected, "{shape:?} {orphans}");
            let steps = plan.confirm(&disk.dump()).unwrap();
            for cut in 0..steps.len() {
                let mut cut_disk = disk.clone();
                for step in &steps[..cut] {
                    cut_disk.apply(step, None, None);
                }
                assert_eq!(cut_disk.header().kind(), Kind::DeviceBound, "{cut}");
            }
            for step in &steps {
                disk.apply(step, None, None);
            }
            // The tokens land as numbers 1 to 3, the model's FIDO_KEY + n.
            assert_eq!(check(&disk.view()), Ok((State::U0, shape)));
            let done = converge(disk, [None, None], true, Resilience::Journal);
            assert_final(&done, shape, full(shape), &format!("{shape:?}"));
        }
    }
    // The TPM shape takes no new key: phase 1 imports `td-upgrade` alone.
    let disk = converged();
    let plan = phase_one(&disk.view(), Shape::Tpm, &[]).unwrap();
    assert_eq!(plan.steps().len(), 1);
}

/// Phase 1's bound covers every header the upgrade boot then reads, each
/// new key at its largest, and is what `phase_one_fits` returns.
#[test]
fn phase_one_fits_bounds_every_later_header() {
    for &shape in SHAPES {
        let mut disk = converged();
        disk.tokens
            .insert(1, Tok::Protected(Token::Fido2(new_key(shape, 8, true))));
        let keys = [new_key(shape, 1, true), new_key(shape, 2, true)];
        let bound = phase_one_fits(&disk.view(), shape, keys.len()).unwrap();
        for step in phase_one(&disk.view(), shape, &keys)
            .unwrap()
            .confirm(&disk.dump())
            .unwrap()
        {
            disk.apply(&step, None, None);
        }
        let mut largest = disk.json_len();
        let mut fresh = 0;
        for _ in 0..4 {
            for steps in 0..32 {
                let mut cut = disk.clone();
                boot(
                    &mut cut,
                    Some(Cut {
                        steps,
                        inside: Some(Partial::Initialized),
                    }),
                    true,
                    Resilience::Checksum,
                    &mut 0,
                );
                largest = largest.max(cut.json_len());
            }
            boot(&mut disk, None, true, Resilience::Checksum, &mut fresh);
        }
        assert_eq!(disk.header().kind(), Kind::Protected);
        assert!(largest <= bound, "{shape:?}: {largest} > {bound}");
    }
    // The model's sizes are below the bound's constants, so its slack
    // would hide a missing term: the bound is exactly the sum, each token
    // at its bounds as a new number.
    let entry = |token: Token| token.encode().unwrap().len() + 6;
    let key = entry(Token::Fido2(Fido2 {
        keyslot: Some(31),
        credential: vec![0; 255],
        ..new_key(Shape::Tpm, 1, false)
    }));
    let pin = entry(Token::TpmPin(TpmPin {
        keyslot: Some(31),
        pcrs: PinPcrs::SecureBoot,
        sealed: SealedObject {
            public: vec![0; 128],
            private: vec![0; 256],
        },
        ..pin(None, 1)
    }));
    let view = converged().view();
    for shape in [Shape::Tpm, Shape::Fido2] {
        for count in 1..=2 {
            let mut sum = count * (key + KEYSLOT_JSON)
                + entry(Token::Upgrade {
                    shape,
                    from: [0; 32],
                })
                + REENCRYPT_JSON
                + entry(Token::RecoveryKey { keyslot: Some(31) });
            if shape == Shape::Tpm {
                sum += KEYSLOT_JSON + pin + LOCKOUT_JSON + 6;
            }
            assert_eq!(
                phase_one_fits(&view, shape, count),
                Ok(view.json_len + sum),
                "{shape:?} {count}"
            );
        }
    }
}

/// Phase 1 refuses a header that is not converged, counts its shape does
/// not take, new keys that are not pending ones of the shape's role, and a
/// header whose upgrade could outgrow the JSON area.
#[test]
fn phase_one_refuses_what_it_cannot_upgrade() {
    let fits = |disk: &Disk| phase_one_fits(&disk.view(), Shape::Tpm, 2);
    assert!(fits(&converged()).is_ok());
    let mut unconverged = vec![Disk::u0(Shape::Tpm), in_state(Shape::Tpm, State::U2)];
    let mut extra = converged();
    extra.keyslots.insert(
        2,
        Slot {
            key: 7,
            vk: 1,
            luks2: true,
        },
    );
    unconverged.push(extra);
    let mut first_boot = converged();
    first_boot.tokens.insert(
        0,
        Tok::Bound {
            slot: Some(1),
            role: Role::FirstBoot,
        },
    );
    unconverged.push(first_boot);
    let mut orphan = converged();
    orphan.tokens.insert(
        4,
        Tok::Bound {
            slot: None,
            role: Role::DeviceBound,
        },
    );
    unconverged.push(orphan);
    let mut foreign = converged();
    foreign.tokens.insert(4, Tok::Foreign(vec![1]));
    unconverged.push(foreign);
    let mut bare = converged();
    bare.tokens.clear();
    unconverged.push(bare);
    let mut zero = converged();
    zero.tokens.insert(
        0,
        Tok::Bound {
            slot: Some(0),
            role: Role::DeviceBound,
        },
    );
    unconverged.push(zero);
    let mut protected = converged();
    protected
        .tokens
        .insert(4, Tok::Protected(Token::RecoveryKey { keyslot: Some(0) }));
    unconverged.push(protected);
    let mut reencrypting = converged();
    reencrypting.reencrypting = Some(2);
    unconverged.push(reencrypting);
    for disk in &unconverged {
        assert_eq!(fits(disk), Err(Refusal::NotConverged), "{disk:?}");
        assert_eq!(
            phase_one(&disk.view(), Shape::Tpm, &[]).err(),
            Some(Refusal::NotConverged)
        );
    }
    let mut no_segment = converged().view();
    no_segment.volume.segment = None;
    assert_eq!(
        phase_one_fits(&no_segment, Shape::Tpm, 0),
        Err(Refusal::NotConverged)
    );
    // Counts: 0 to 2 in the TPM shape, 1 or 2 in the FIDO2 shape.
    let view = converged().view();
    for (shape, count, admitted) in [
        (Shape::Tpm, 0, true),
        (Shape::Tpm, 2, true),
        (Shape::Tpm, 3, false),
        (Shape::Fido2, 0, false),
        (Shape::Fido2, 1, true),
        (Shape::Fido2, 3, false),
    ] {
        let fitted = phase_one_fits(&view, shape, count);
        assert_eq!(fitted.is_ok(), admitted, "{shape:?} {count}");
        if !admitted {
            assert_eq!(fitted, Err(Refusal::NewKeys));
        }
    }
    // New keys: pending, of the shape's role.
    for key in [
        Fido2 {
            keyslot: Some(2),
            ..new_key(Shape::Tpm, 1, false)
        },
        new_key(Shape::Fido2, 1, false),
    ] {
        assert_eq!(
            phase_one(&view, Shape::Tpm, &[key]).err(),
            Some(Refusal::NewKeys)
        );
    }
    let long = Fido2 {
        credential: vec![1; MAX_CREDENTIAL_ID + 1],
        ..new_key(Shape::Tpm, 1, false)
    };
    assert!(matches!(
        phase_one(&view, Shape::Tpm, &[long]),
        Err(Refusal::Token(_))
    ));
    // The bound must leave the area's last byte.
    let growth = phase_one_fits(&view, Shape::Tpm, 2).unwrap() - view.json_len;
    let mut edge = view.clone();
    edge.json_len = JSON_AREA_LEN - 1 - growth;
    assert_eq!(phase_one_fits(&edge, Shape::Tpm, 2), Ok(JSON_AREA_LEN - 1));
    edge.json_len += 1;
    assert_eq!(
        phase_one_fits(&edge, Shape::Tpm, 2),
        Err(Refusal::TooLarge {
            bytes: JSON_AREA_LEN
        })
    );
    assert!(phase_one(&edge, Shape::Tpm, &[new_key(Shape::Tpm, 1, false)]).is_ok());
    // Each new key, the TPM shape's protectors and the shape's own token
    // grow the bound.
    let one = phase_one_fits(&view, Shape::Tpm, 1).unwrap();
    assert!(phase_one_fits(&view, Shape::Tpm, 0).unwrap() < one);
    assert!(phase_one_fits(&view, Shape::Fido2, 1).unwrap() < one);
}
