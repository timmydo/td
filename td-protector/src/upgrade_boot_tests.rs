use super::*;
use crate::cryptsetup::Progress;
use crate::protected_release::tests::{exited, note, Log, Model};
use crate::protected_release::{self, Entered, Entry};
use crate::release::Runner;
use crate::upgrade::tests::{Disk, Partial, Tok, FIDO_KEY, RECOVERY_KEY};
use std::cell::RefCell;
use std::ffi::OsString;
use std::rc::Rc;

const DEVICE: &str = "/proc/1/fd/5";
const NAME: &str = "td-selector";
const RECOVERY: &[u8] = b"123459123459123459123459123459123459123459123459";
const NEW_PIN: &[u8] = b"battery staple";

/// A pending fido2 token `n`'s passphrase as the security key derives it.
fn passphrase(n: u8) -> [u8; 32] {
    [0x40 + n; 32]
}

/// The disk the runner and the host share, the bytes each model key is,
/// and the boot's commands.
struct World {
    disk: Disk,
    /// The model key each secret this test saw stands for.
    keys: Vec<(Vec<u8>, u32)>,
    log: Log,
    /// The mapping the host opened, by the keyslot it named.
    opened: Option<Option<u8>>,
    commands: usize,
    /// The command of this index, and every later one, fails, as a power
    /// cut stops the boot there.
    cut: Option<usize>,
    /// Where a cut at a reencryption stops it inside.
    inside: Option<Partial>,
    /// What changes in the TPM at the first command, after this boot's
    /// seal.
    after_seal: Option<Box<dyn FnOnce()>>,
}

type Shared = Rc<RefCell<World>>;

fn world(disk: Disk, log: &Log) -> Shared {
    Rc::new(RefCell::new(World {
        disk,
        keys: vec![(RECOVERY.to_vec(), RECOVERY_KEY)],
        log: log.clone(),
        opened: None,
        commands: 0,
        cut: None,
        inside: None,
        after_seal: None,
    }))
}

impl World {
    /// The model key `bytes` is: the recovery key, a pending key's
    /// passphrase, or a fresh one for a TPM secret.
    fn key(&mut self, bytes: &[u8]) -> u32 {
        if let Some((_, key)) = self.keys.iter().find(|(known, _)| known == bytes) {
            return *key;
        }
        let key = (1..=9u8)
            .find(|n| bytes == passphrase(*n))
            .map(|n| FIDO_KEY + u32::from(n))
            .unwrap_or(5000 + self.keys.len() as u32);
        self.keys.push((bytes.to_vec(), key));
        key
    }

    /// The step key `key` stands for, and the TPM secret it is when it is
    /// neither the recovery key nor a pending key's.
    fn step_key(key: u32) -> (Key, Option<u32>) {
        if key == RECOVERY_KEY {
            (Key::Recovery, None)
        } else if (FIDO_KEY + 1..FIDO_KEY + 10).contains(&key) {
            (
                Key::Fido2 {
                    token: (key - FIDO_KEY) as u8,
                },
                None,
            )
        } else {
            (Key::TpmPin, Some(key))
        }
    }

    /// A luks2 keyslot of the data segment's key that `key` opens.
    fn opening(&self, key: u32, except: u8) -> Option<u8> {
        self.disk
            .keyslots
            .keys()
            .copied()
            .find(|slot| *slot != except && self.disk.opens(*slot, key))
    }
}

fn value(words: &[String], flag: &str) -> Option<u8> {
    let at = words.iter().position(|word| word == flag)?;
    words.get(at + 1)?.parse().ok()
}

struct Cryptsetup(Shared);

impl Runner for Cryptsetup {
    fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
        let words: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let mut world = self.0.borrow_mut();
        assert!(words.iter().any(|word| word == DEVICE), "{words:?}");
        if let Some(change) = world.after_seal.take() {
            change();
        }
        let index = world.commands;
        world.commands += 1;
        // From the cut on the machine is off: nothing more commits, but a
        // reencryption cut inside commits what it reached.
        if world.cut.is_some_and(|cut| index >= cut) {
            note(&world.log, format!("cs cut {}", words[0]));
            if world.cut == Some(index) && words[0] == "reencrypt" {
                if let Some(partial) = world.inside {
                    let step = if words.iter().any(|word| word == "--resume-only") {
                        Step::ResumeReencrypt
                    } else {
                        Step::Reencrypt {
                            slot: value(&words, "--key-slot").unwrap(),
                            resilience: Resilience::Journal,
                        }
                    };
                    world.disk.apply(&step, None, Some(partial));
                }
            }
            return Err(io::Error::other("power cut"));
        }
        let key = world.key(input);
        let step = match (words[0].as_str(), words.get(1).map(String::as_str)) {
            ("open", Some("--test-passphrase")) => {
                let slot = value(&words, "--key-slot").unwrap();
                let ok = world.disk.opens(slot, key);
                note(
                    &world.log,
                    format!("cs test {slot} {}", if ok { "ok" } else { "bad" }),
                );
                return if ok { Ok(()) } else { Err(exited(2)) };
            }
            ("luksKillSlot", _) => {
                let slot: u8 = words.last().unwrap().parse().unwrap();
                let Some(by) = world.opening(key, slot) else {
                    return Err(exited(2));
                };
                Step::KillSlot { slot, by }
            }
            ("luksAddKey", _) => {
                let by = value(&words, "--key-slot").unwrap();
                let slot = value(&words, "--new-key-slot").unwrap();
                if !world.disk.opens(by, key) {
                    return Err(exited(2));
                }
                let new = std::fs::read(words.last().unwrap()).unwrap();
                let (key, tpm) = World::step_key(world.key(&new));
                let step = Step::AddKeyslot { slot, by, key };
                world.disk.apply(&step, tpm, None);
                note(&world.log, format!("cs add {slot}"));
                return Ok(());
            }
            ("token", Some("remove")) => Step::RemoveToken {
                token: value(&words, "--token-id").unwrap(),
            },
            ("token", Some("import")) => Step::ImportToken {
                token: Token::decode(input).unwrap(),
                id: value(&words, "--token-id"),
                replace: words.iter().any(|word| word == "--token-replace"),
            },
            ("reencrypt", _) => {
                assert_eq!(key, RECOVERY_KEY);
                assert!(world.opened.is_some(), "a reencryption with no mapping");
                assert!(words.iter().any(|word| word == "--progress-json"));
                if words.iter().any(|word| word == "--resume-only") {
                    Step::ResumeReencrypt
                } else {
                    let resilience = if words.iter().any(|word| word == "checksum") {
                        Resilience::Checksum
                    } else {
                        Resilience::Journal
                    };
                    Step::Reencrypt {
                        slot: value(&words, "--key-slot").unwrap(),
                        resilience,
                    }
                }
            }
            other => panic!("unexpected cryptsetup command {other:?}"),
        };
        let (_, tpm) = World::step_key(key);
        world.disk.apply(&step, tpm, None);
        note(&world.log, format!("cs {}", words[..2].join(" ")));
        Ok(())
    }

    fn metadata(&mut self, device: &Path) -> io::Result<Vec<u8>> {
        assert_eq!(device, Path::new(DEVICE));
        let world = self.0.borrow();
        // Off once the cut command failed.
        if world.cut.is_some_and(|cut| world.commands > cut) {
            return Err(io::Error::other("power cut"));
        }
        Ok(world.disk.dump())
    }

    fn reencrypt(
        &mut self,
        args: &[OsString],
        input: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> io::Result<()> {
        for done in [0, 3, 5, 5, 40, 100, 130] {
            progress(Progress::Bytes { done, size: 100 });
        }
        progress(Progress::Unreadable);
        self.run(args, input)
    }
}

/// The volume key the model hands over: its generation's byte.
struct VolumeKey(Vec<u8>);

impl AsRef<[u8]> for VolumeKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

struct Selector(Shared);

impl Host for Selector {
    type VolumeKey = VolumeKey;

    fn read(&mut self) -> Result<UpgradeView, String> {
        Ok(self.0.borrow().disk.view())
    }

    fn open(&mut self, slot: Option<u8>, key: &[u8]) -> io::Result<()> {
        let mut world = self.0.borrow_mut();
        assert!(world.opened.is_none(), "the mapping opened twice");
        let key = world.key(key);
        let opens = match slot {
            Some(slot) => world.disk.opens(slot, key),
            // Mid reencryption cryptsetup opens a keyslot of each key.
            None => {
                world.disk.reencrypting.is_some()
                    && world
                        .disk
                        .keyslots
                        .values()
                        .any(|s| s.luks2 && s.key == key)
            }
        };
        if !opens {
            return Err(exited(2));
        }
        note(&world.log, format!("host open {slot:?}"));
        world.opened = Some(slot);
        Ok(())
    }

    fn volume_key(&mut self, key: &[u8]) -> io::Result<VolumeKey> {
        let mut world = self.0.borrow_mut();
        assert!(world.opened.is_some(), "a volume key with no mapping");
        assert!(
            world.disk.reencrypting.is_none(),
            "a volume key mid reencryption"
        );
        let key = world.key(key);
        assert!(world.opening(key, u8::MAX).is_some());
        note(&world.log, "host volume key".into());
        Ok(VolumeKey(vec![world.disk.vk as u8; 64]))
    }
}

/// The security keys: each token's key answers, but a `missing` one
/// answers nothing the first time it is asked.
struct Keys {
    asked: Vec<u8>,
    missing: Vec<u8>,
    log: Log,
}

impl Fido for Keys {
    fn release(
        &mut self,
        tokens: &[FidoToken<'_>],
        _: &[u8; 16],
        description: &[u8],
        _: &mut dyn Prompt,
        _: &mut dyn FnMut(&str),
    ) -> Result<(usize, Secret), FidoError> {
        assert_eq!(tokens.len(), 1, "a pending key is asked alone");
        assert_eq!(
            description,
            protected_release::unlock_description("0f0e0d0c-0b0a-4908-8706-050403020100")
        );
        let number = tokens[0].number;
        self.asked.push(number);
        note(&self.log, format!("key {number}"));
        if let Some(at) = self.missing.iter().position(|missing| *missing == number) {
            self.missing.remove(at);
            return Err(FidoError::Retry("td: no security key answered".into()));
        }
        Ok((0, Secret::copy_of(&passphrase(number))))
    }
}

/// The console's answers, by the prompt they answer.
struct Person {
    recovery: Vec<&'static [u8]>,
    /// The protected release's PIN entries, the new PIN's after them.
    pin: Vec<&'static [u8]>,
    keys: Vec<&'static [u8]>,
    cancel: Vec<&'static [u8]>,
    /// End of input at the first prompt starting with this.
    eof: Option<&'static str>,
    asked: Vec<String>,
}

impl Person {
    fn new() -> Self {
        Self {
            recovery: Vec::new(),
            pin: Vec::new(),
            keys: Vec::new(),
            cancel: Vec::new(),
            eof: None,
            asked: Vec::new(),
        }
    }
}

impl Prompt for Person {
    fn read(&mut self, prompt: &str) -> Result<Entered, String> {
        self.asked.push(prompt.to_owned());
        if self.eof.is_some_and(|eof| prompt.starts_with(eof)) {
            return Ok(Entered::Eof);
        }
        let next = |list: &mut Vec<&'static [u8]>, default: Option<&'static [u8]>| {
            if list.is_empty() {
                default
            } else {
                Some(list.remove(0))
            }
        };
        let answer = if prompt == RECOVERY_PROMPT
            || prompt == RECOVERY_AGAIN_PROMPT
            || prompt == protected_release::ANOTHER_WAY_PROMPT
        {
            next(&mut self.recovery, Some(RECOVERY))
        } else if prompt == protected_release::PIN_PROMPT {
            next(&mut self.pin, Some(NEW_PIN))
        } else if prompt == protected_release::RESEAL_PROMPT {
            Some(&b"no"[..])
        } else if prompt == CANCEL_PROMPT {
            next(&mut self.cancel, None)
        } else if prompt.starts_with("td new security key") {
            next(&mut self.keys, Some(b""))
        } else if prompt == protected_release::NEW_PIN_PROMPT
            || prompt == protected_release::NEW_PIN_AGAIN_PROMPT
        {
            Some(NEW_PIN)
        } else {
            panic!("unexpected prompt {prompt}")
        };
        Ok(match answer {
            Some(bytes) => Entered::Line(Entry::new(bytes)),
            None => Entered::Eof,
        })
    }
}

/// One boot's outcome and its console.
struct Booted {
    outcome: Outcome<VolumeKey>,
    console: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn boot_once(
    shared: &Shared,
    tpm: Option<&mut Model>,
    keys: &mut Keys,
    person: &mut Person,
    on_mains: bool,
) -> Booted {
    let view = shared.borrow().disk.view();
    let mut console = Vec::new();
    let boot = Boot {
        device: Path::new(DEVICE),
        name: NAME,
        on_mains,
        resilience: Resilience::Journal,
    };
    let outcome = upgrade(
        &view,
        &boot,
        tpm,
        &mut Cryptsetup(shared.clone()),
        keys,
        &mut Selector(shared.clone()),
        person,
        &mut |line| console.push(line.to_owned()),
    );
    Booted { outcome, console }
}

fn security_keys(log: &Log) -> Keys {
    Keys {
        asked: Vec::new(),
        missing: Vec::new(),
        log: log.clone(),
    }
}

/// The protected header the upgrade ends at: the shape, the recovery
/// key's marker, `fido` fido2 tokens naming keyslots, and `tpm` tpm-pin
/// and lockout tokens, the volume key rotated.
fn assert_protected(disk: &Disk, shape: Shape, fido: usize, tpm: (usize, usize)) {
    let header = disk.header();
    assert_eq!(header.kind(), Kind::Protected, "{disk:?}");
    assert_eq!(protected_plan::check(&header), Ok(shape));
    assert!(header.tokens.is_empty() && header.orphans.is_empty());
    assert_ne!(disk.vk, 1, "the volume key did not rotate");
    let count = |pick: fn(&Token) -> bool| header.protected.iter().filter(|(_, t)| pick(t)).count();
    assert_eq!(
        count(|t| matches!(t, Token::Fido2(f) if f.keyslot.is_some())),
        fido
    );
    assert_eq!(count(|t| matches!(t, Token::Fido2(_))), fido);
    assert_eq!(
        count(|t| matches!(t, Token::RecoveryKey { keyslot: Some(_) })),
        1
    );
    assert_eq!(
        (
            count(|t| matches!(t, Token::TpmPin(p) if p.keyslot.is_some())),
            count(|t| matches!(t, Token::Lockout(_)))
        ),
        tpm
    );
}

/// The whole upgrade boot from U0 in each shape: the recovery key first,
/// each pending key alone, in the TPM shape the new PIN twice and the
/// seal before the cap; after it every state's plan, the progress, the
/// mapping opened before the reencryption and the volume key taken after
/// it, and in the TPM shape the take; handed off with the rotated key.
#[test]
fn the_upgrade_boot_protects_the_disk_in_each_shape() {
    for &shape in &[Shape::Tpm, Shape::Fido2] {
        let log = Log::default();
        let shared = world(Disk::u0(shape), &log);
        let mut model = Model::new(&log);
        let mut keys = security_keys(&log);
        let mut person = Person::new();
        let booted = boot_once(&shared, Some(&mut model), &mut keys, &mut person, true);
        let Outcome::Upgraded { key } = booted.outcome else {
            panic!("{shape:?}: {:?}", booted.console);
        };
        let world = shared.borrow();
        assert_eq!(key.0, vec![world.disk.vk as u8; 64]);
        assert_eq!(keys.asked, [1, 2]);
        let tpm = match shape {
            Shape::Tpm => (1, 1),
            Shape::Fido2 => (0, 0),
        };
        assert_protected(&world.disk, shape, 2, tpm);
        assert_eq!(
            person.asked.first().map(String::as_str),
            Some(RECOVERY_PROMPT)
        );
        let pins = person
            .asked
            .iter()
            .filter(|p| *p == protected_release::NEW_PIN_PROMPT)
            .count();
        assert_eq!(pins, usize::from(shape == Shape::Tpm), "{shape:?}");
        let log = log.borrow();
        let at = |entry: &str| log.iter().position(|e| e.starts_with(entry));
        // The keys and the seal before any cryptsetup command, the mapping
        // before the reencryption, the key after it.
        let first = at("cs ").unwrap();
        assert!(at("key 2").unwrap() < first);
        if shape == Shape::Tpm {
            assert!(at("tpm seal").unwrap() < first);
            assert!(at("tpm lockout Take").is_some());
        }
        assert!(at("host open Some(0)").unwrap() < at("cs reencrypt").unwrap());
        assert!(at("cs reencrypt").unwrap() < at("host volume key").unwrap());
        let progress: Vec<&String> = booted
            .console
            .iter()
            .filter(|line| line.starts_with("td: re-encrypting"))
            .collect();
        assert_eq!(
            progress,
            [
                "td: re-encrypting the disk: 0%",
                "td: re-encrypting the disk: 3%",
                "td: re-encrypting the disk: 5%",
                "td: re-encrypting the disk: 40%",
                "td: re-encrypting the disk: 100%"
            ]
        );
        assert!(booted
            .console
            .iter()
            .any(|line| line == "PCR 12 release cap closed"));
    }
}

/// U0 without mains power asks nothing and writes nothing; after U0 a
/// battery boot warns and continues.
#[test]
fn u0_waits_for_mains_power() {
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Fido2), &log);
    let mut person = Person::new();
    let booted = boot_once(&shared, None, &mut security_keys(&log), &mut person, false);
    assert!(matches!(booted.outcome, Outcome::NoMains));
    assert_eq!(booted.console, [NO_MAINS]);
    assert!(person.asked.is_empty());
    assert_eq!(shared.borrow().commands, 0);
    // Cut after U0's commits: U1 runs on battery with the warning.
    shared.borrow_mut().cut = Some(3);
    let booted = boot_once(
        &shared,
        None,
        &mut security_keys(&log),
        &mut Person::new(),
        true,
    );
    assert!(matches!(booted.outcome, Outcome::Halt(_)));
    let mut world = shared.borrow_mut();
    world.cut = None;
    world.opened = None;
    drop(world);
    let booted = boot_once(
        &shared,
        None,
        &mut security_keys(&log),
        &mut Person::new(),
        false,
    );
    assert!(
        matches!(booted.outcome, Outcome::Upgraded { .. }),
        "{:?}",
        booted.console
    );
    assert!(booted.console.iter().any(|line| line == ON_BATTERY));
}

/// In U0 an empty recovery-key entry asks whether to cancel: exactly
/// `cancel` cancels, before any key, seal or command, and the
/// cancellation then leaves a device-bound header; any other answer
/// returns to the prompt.
#[test]
fn u0_cancels_at_the_recovery_keys_empty_entry() {
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Tpm), &log);
    let mut model = Model::new(&log);
    let mut keys = security_keys(&log);
    let mut person = Person::new();
    person.recovery = vec![b"", b""];
    person.cancel = vec![b"Cancel", b"cancel"];
    let booted = boot_once(&shared, Some(&mut model), &mut keys, &mut person, true);
    assert!(matches!(booted.outcome, Outcome::Cancel));
    assert_eq!(
        person.asked,
        [
            RECOVERY_PROMPT,
            CANCEL_PROMPT,
            RECOVERY_PROMPT,
            CANCEL_PROMPT
        ]
    );
    assert!(keys.asked.is_empty());
    assert!(!log.borrow().iter().any(|e| e.starts_with("tpm ")));
    assert_eq!(shared.borrow().commands, 0);
    let view = shared.borrow().disk.view();
    let mut console = Vec::new();
    cancel(
        &view,
        Path::new(DEVICE),
        &mut Cryptsetup(shared.clone()),
        &mut |line| console.push(line.to_owned()),
    );
    let header = shared.borrow().disk.header();
    assert_eq!(header.kind(), Kind::DeviceBound, "{console:?}");
    assert!(header.protected.is_empty());
}

/// After U0 nothing cancels: an empty entry says so and asks again.
#[test]
fn after_u0_nothing_cancels() {
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Fido2), &log);
    shared.borrow_mut().cut = Some(3);
    assert!(matches!(
        boot_once(
            &shared,
            None,
            &mut security_keys(&log),
            &mut Person::new(),
            true
        )
        .outcome,
        Outcome::Halt(_)
    ));
    let mut world = shared.borrow_mut();
    world.cut = None;
    world.opened = None;
    drop(world);
    let mut person = Person::new();
    person.recovery = vec![b""];
    let booted = boot_once(&shared, None, &mut security_keys(&log), &mut person, true);
    assert!(matches!(booted.outcome, Outcome::Upgraded { .. }));
    assert_eq!(
        &person.asked[..2],
        [RECOVERY_AGAIN_PROMPT, RECOVERY_AGAIN_PROMPT]
    );
    assert!(booted.console.iter().any(|line| line == NO_CANCEL));
}

/// A recovery key that does not open its keyslot after the cap asks for
/// it again, keeping the keys and the seal, and no longer cancels.
#[test]
fn a_wrong_recovery_key_asks_again_after_the_cap() {
    let other = crate::recovery::RecoveryKey::generate()
        .unwrap()
        .passphrase();
    let other: &'static [u8] = Box::leak(other.expose().to_vec().into_boxed_slice());
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Tpm), &log);
    let mut model = Model::new(&log);
    let mut keys = security_keys(&log);
    let mut person = Person::new();
    person.recovery = vec![other, b""];
    let booted = boot_once(&shared, Some(&mut model), &mut keys, &mut person, true);
    assert!(
        matches!(booted.outcome, Outcome::Upgraded { .. }),
        "{:?}",
        booted.console
    );
    assert_eq!(keys.asked, [1, 2]);
    let seals = log.borrow().iter().filter(|e| *e == "tpm seal").count();
    assert_eq!(seals, 1);
    assert_eq!(
        person
            .asked
            .iter()
            .filter(|p| p.as_str() == RECOVERY_AGAIN_PROMPT)
            .count(),
        2
    );
    assert!(!person.asked.iter().any(|p| p == CANCEL_PROMPT));
    assert!(booted.console.iter().any(|line| line == NO_CANCEL));
    assert_protected(&shared.borrow().disk, Shape::Tpm, 2, (1, 1));
}

/// `drop` at a pending key's prompt leaves its token out, in the TPM
/// shape for any key; the FIDO2 shape's last fido2-primary key cannot be
/// dropped; other entries and a key that answers nothing ask again.
#[test]
fn a_pending_key_may_be_dropped_but_not_the_last_primary() {
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Tpm), &log);
    let mut model = Model::new(&log);
    let mut keys = security_keys(&log);
    let mut person = Person::new();
    person.keys = vec![b"later", b"drop"];
    let booted = boot_once(&shared, Some(&mut model), &mut keys, &mut person, true);
    assert!(
        matches!(booted.outcome, Outcome::Upgraded { .. }),
        "{:?}",
        booted.console
    );
    assert_eq!(keys.asked, [2]);
    assert_protected(&shared.borrow().disk, Shape::Tpm, 1, (1, 1));

    let log = Log::default();
    let shared = world(Disk::u0(Shape::Fido2), &log);
    let mut keys = security_keys(&log);
    keys.missing = vec![1];
    let mut person = Person::new();
    // Token 1 is the shape's one fido2-primary key: its drop is refused,
    // then its key answers nothing once, then answers.
    person.keys = vec![b"drop", b"", b""];
    let booted = boot_once(&shared, None, &mut keys, &mut person, true);
    assert!(
        matches!(booted.outcome, Outcome::Upgraded { .. }),
        "{:?}",
        booted.console
    );
    assert_eq!(keys.asked, [1, 1, 2]);
    assert!(booted
        .console
        .iter()
        .any(|line| line.contains("cannot be dropped")));
    assert!(booted
        .console
        .iter()
        .any(|line| line == "td: no security key answered"));
    assert_protected(&shared.borrow().disk, Shape::Fido2, 2, (0, 0));
}

/// A TPM whose seal fails, or whose lockout authorization another system
/// set, is unusable for td: the TPM shape completes without the TPM PIN
/// and takes nothing, the console saying why; with no TPM the same.
#[test]
fn an_unusable_tpm_completes_without_the_tpm_pin() {
    for case in ["seal", "lockout", "absent"] {
        let log = Log::default();
        let shared = world(Disk::u0(Shape::Tpm), &log);
        let mut model = Model::new(&log);
        match case {
            "seal" => model.state.borrow_mut().seal_fault = Some("fail"),
            "lockout" => model.state.borrow_mut().da.lockout_auth_set = true,
            _ => {}
        }
        let mut person = Person::new();
        let tpm = (case != "absent").then_some(&mut model);
        let booted = boot_once(&shared, tpm, &mut security_keys(&log), &mut person, true);
        assert!(
            matches!(booted.outcome, Outcome::Upgraded { .. }),
            "{case}: {:?}",
            booted.console
        );
        assert_protected(&shared.borrow().disk, Shape::Tpm, 2, (0, 0));
        assert!(
            booted
                .console
                .iter()
                .any(|line| line.starts_with("td: the TPM is unusable for td")),
            "{case}"
        );
        let pins = person
            .asked
            .iter()
            .filter(|p| *p == protected_release::NEW_PIN_PROMPT)
            .count();
        assert_eq!(pins, usize::from(case == "seal"), "{case}");
        assert!(
            !log.borrow().iter().any(|e| e.starts_with("tpm lockout")),
            "{case}"
        );
    }
}

/// A primary whose Name changed between the seal and the take, across a
/// cut after the tpm-pin import: the take is not sent, and the new
/// protector's keyslot and token are removed.
#[test]
fn a_changed_primary_at_the_take_removes_the_new_protector() {
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Tpm), &log);
    let mut model = Model::new(&log);
    // Every command of the uncut boot, the last being the lockout import.
    let whole = {
        let log = Log::default();
        let probe = world(Disk::u0(Shape::Tpm), &log);
        let mut model = Model::new(&log);
        boot_once(
            &probe,
            Some(&mut model),
            &mut security_keys(&log),
            &mut Person::new(),
            true,
        );
        let commands = probe.borrow().commands;
        commands
    };
    // Cut at the lockout token's import, the take's first write.
    shared.borrow_mut().cut = Some(whole - 3);
    let booted = boot_once(
        &shared,
        Some(&mut model),
        &mut security_keys(&log),
        &mut Person::new(),
        true,
    );
    assert!(
        matches!(booted.outcome, Outcome::Halt(_)),
        "{:?}",
        booted.console
    );
    let header = shared.borrow().disk.header();
    assert!(header
        .protected
        .iter()
        .any(|(_, t)| matches!(t, Token::TpmPin(p) if p.keyslot.is_some())));
    assert!(!header
        .protected
        .iter()
        .any(|(_, t)| matches!(t, Token::Lockout(_))));
    let mut world = shared.borrow_mut();
    world.cut = None;
    world.opened = None;
    drop(world);
    model.reboot();
    model.state.borrow_mut().primary = [0x0c; 34];
    let booted = boot_once(
        &shared,
        Some(&mut model),
        &mut security_keys(&log),
        &mut Person::new(),
        true,
    );
    assert!(
        matches!(booted.outcome, Outcome::Upgraded { .. }),
        "{:?}",
        booted.console
    );
    assert!(booted
        .console
        .iter()
        .any(|line| line.contains("storage primary is not the one")));
    assert_protected(&shared.borrow().disk, Shape::Tpm, 2, (0, 0));
    assert!(!log.borrow().iter().any(|e| e.starts_with("tpm lockout")));
}

/// Every cut of the upgrade boot's commands, in both shapes: the boots
/// after it converge on one protected header.
#[test]
fn every_cut_converges() {
    for &shape in &[Shape::Tpm, Shape::Fido2] {
        let whole = {
            let log = Log::default();
            let probe = world(Disk::u0(shape), &log);
            let mut model = Model::new(&log);
            boot_once(
                &probe,
                Some(&mut model),
                &mut security_keys(&log),
                &mut Person::new(),
                true,
            );
            let commands = probe.borrow().commands;
            commands
        };
        for cut in 0..whole {
            let log = Log::default();
            let shared = world(Disk::u0(shape), &log);
            let mut model = Model::new(&log);
            shared.borrow_mut().cut = Some(cut);
            let mut ended = false;
            for boot in 0..4 {
                let booted = boot_once(
                    &shared,
                    Some(&mut model),
                    &mut security_keys(&log),
                    &mut Person::new(),
                    true,
                );
                let mut world = shared.borrow_mut();
                world.cut = None;
                world.opened = None;
                drop(world);
                model.reboot();
                // A cut before the last step leaves U5, whose end the
                // protected release's boot runs.
                let view = shared.borrow().disk.view();
                if matches!(upgrade::check(&view), Ok((State::U5, _))) {
                    assert!(finish_u5(
                        &view,
                        Path::new(DEVICE),
                        &mut Cryptsetup(shared.clone()),
                        &mut |_| {}
                    ));
                }
                if shared.borrow().disk.header().kind() == Kind::Protected {
                    ended = true;
                    break;
                }
                assert!(
                    matches!(booted.outcome, Outcome::Halt(_)),
                    "{shape:?} cut {cut} boot {boot}: {:?}",
                    booted.console
                );
            }
            assert!(ended, "{shape:?} cut {cut}");
            let header = shared.borrow().disk.header();
            assert_eq!(
                protected_plan::check(&header),
                Ok(shape),
                "{shape:?} cut {cut}"
            );
        }
    }
}

/// A cut inside the reencryption: before its initialization's commit
/// the next boot kills the unbound keyslot and starts again; after it the
/// header is U2, whose boot opens the mapping naming no keyslot before the
/// resumption, and a resumption cut inside resumes again.
#[test]
fn a_cut_inside_the_reencryption_resumes() {
    for inside in [Partial::Unbound, Partial::Initialized] {
        let log = Log::default();
        let shared = world(Disk::u0(Shape::Fido2), &log);
        // The reencryption is the U1 round's last command, after U0's three
        // and U1's test.
        {
            let mut world = shared.borrow_mut();
            world.cut = Some(4);
            world.inside = Some(inside);
        }
        let booted = boot_once(
            &shared,
            None,
            &mut security_keys(&log),
            &mut Person::new(),
            true,
        );
        assert!(
            matches!(booted.outcome, Outcome::Halt(_)),
            "{:?}",
            booted.console
        );
        let state = upgrade::check(&shared.borrow().disk.view()).map(|(state, _)| state);
        let expected = match inside {
            Partial::Initialized => State::U2,
            _ => State::U1,
        };
        assert_eq!(state, Ok(expected), "{inside:?}");
        if inside == Partial::Initialized {
            // The resumption, itself cut inside, then resumed again.
            let mut world = shared.borrow_mut();
            world.cut = Some(world.commands);
            world.opened = None;
            drop(world);
            let booted = boot_once(
                &shared,
                None,
                &mut security_keys(&log),
                &mut Person::new(),
                true,
            );
            assert!(
                matches!(booted.outcome, Outcome::Halt(_)),
                "{:?}",
                booted.console
            );
            assert!(log.borrow().iter().any(|e| e == "host open None"));
            let state = upgrade::check(&shared.borrow().disk.view()).map(|(state, _)| state);
            assert_eq!(state, Ok(State::U2));
        }
        let mut world = shared.borrow_mut();
        world.cut = None;
        world.opened = None;
        drop(world);
        let booted = boot_once(
            &shared,
            None,
            &mut security_keys(&log),
            &mut Person::new(),
            true,
        );
        assert!(
            matches!(booted.outcome, Outcome::Upgraded { .. }),
            "{inside:?}: {:?}",
            booted.console
        );
        assert_protected(&shared.borrow().disk, Shape::Fido2, 2, (0, 0));
    }
}

/// A U5 header, which a cut before the last step leaves, is the protected
/// release's: `release_upgraded` admits it and only it, and `finish_u5`
/// removes the `td-upgrade` token, after which the header is protected.
#[test]
fn u5_releases_as_a_protected_boot_and_removes_td_upgrade() {
    let log = Log::default();
    let whole = {
        let probe = world(Disk::u0(Shape::Fido2), &log);
        boot_once(
            &probe,
            None,
            &mut security_keys(&log),
            &mut Person::new(),
            true,
        );
        let commands = probe.borrow().commands;
        commands
    };
    let shared = world(Disk::u0(Shape::Fido2), &log);
    shared.borrow_mut().cut = Some(whole - 1);
    boot_once(
        &shared,
        None,
        &mut security_keys(&log),
        &mut Person::new(),
        true,
    );
    shared.borrow_mut().cut = None;
    let view = shared.borrow().disk.view();
    assert_eq!(upgrade::check(&view).map(|(state, _)| state), Ok(State::U5));
    // Before U5 the release refuses the header.
    let early = Disk::u0(Shape::Fido2).view();
    let mut person = Person::new();
    let refused = protected_release::release_upgraded::<Model, _, _>(
        &early,
        Path::new(DEVICE),
        None,
        &mut Cryptsetup(shared.clone()),
        &mut security_keys(&log),
        &mut person,
        &mut |_| None,
        &mut |_| {},
    );
    assert!(matches!(refused, protected_release::Outcome::Halt(_)));
    // In U5 it releases, a protected header's release, offering no
    // reseal: the TPM shape's by the TPM PIN.
    let tpm_log = Log::default();
    let whole = {
        let probe = world(Disk::u0(Shape::Tpm), &tpm_log);
        let mut model = Model::new(&tpm_log);
        boot_once(
            &probe,
            Some(&mut model),
            &mut security_keys(&tpm_log),
            &mut Person::new(),
            true,
        );
        let commands = probe.borrow().commands;
        commands
    };
    let tpm_shared = world(Disk::u0(Shape::Tpm), &tpm_log);
    let mut model = Model::new(&tpm_log);
    tpm_shared.borrow_mut().cut = Some(whole - 1);
    boot_once(
        &tpm_shared,
        Some(&mut model),
        &mut security_keys(&tpm_log),
        &mut Person::new(),
        true,
    );
    tpm_shared.borrow_mut().cut = None;
    model.reboot();
    let u5 = tpm_shared.borrow().disk.view();
    assert_eq!(upgrade::check(&u5).map(|(state, _)| state), Ok(State::U5));
    let mut person = Person::new();
    let released = protected_release::release_upgraded(
        &u5,
        Path::new(DEVICE),
        Some(&mut model),
        &mut Cryptsetup(tpm_shared.clone()),
        &mut security_keys(&tpm_log),
        &mut person,
        &mut |_| None,
        &mut |_| {},
    );
    let protected_release::Outcome::Released(released) = released else {
        panic!("U5 did not release");
    };
    assert_eq!(released.method, protected_release::Method::TpmPin);
    assert_eq!(person.asked, [protected_release::PIN_PROMPT]);
    // On a changed chain the recovery key's release, which offers a
    // protected header the reseal, offers U5 none.
    let reseal_asked = |view: &UpgradeView, model: &mut Model| {
        model.reboot();
        model.wire.0.borrow_mut().pcrs[4] = [0x50; 32];
        let mut lines = Vec::new();
        let mut person = Person::new();
        person.pin = vec![b""];
        let mut release = |person: &mut Person, model: &mut Model| {
            let console = &mut |line: &str| lines.push(line.to_owned());
            let keys = &mut security_keys(&tpm_log);
            let runner = &mut Cryptsetup(tpm_shared.clone());
            if view.header.kind() == Kind::Upgrading {
                protected_release::release_upgraded(
                    view,
                    Path::new(DEVICE),
                    Some(model),
                    runner,
                    keys,
                    person,
                    &mut |_| None,
                    console,
                )
            } else {
                protected_release::release(
                    &view.header,
                    Path::new(DEVICE),
                    Some(model),
                    runner,
                    keys,
                    person,
                    &mut |_| None,
                    console,
                )
            }
        };
        let released = release(&mut person, model);
        assert!(matches!(
            released,
            protected_release::Outcome::Released(ref r)
                if r.method == protected_release::Method::RecoveryKey
        ));
        let asked = person
            .asked
            .iter()
            .any(|p| p == protected_release::RESEAL_PROMPT);
        (asked, lines)
    };
    let (asked, lines) = reseal_asked(&u5, &mut model);
    assert!(!asked);
    assert!(
        lines.iter().any(|l| l.starts_with("no reseal is offered")),
        "{lines:?}"
    );
    assert!(finish_u5(
        &u5,
        Path::new(DEVICE),
        &mut Cryptsetup(tpm_shared.clone()),
        &mut |_| {}
    ));
    let protected = tpm_shared.borrow().disk.view();
    let (asked, lines) = reseal_asked(&protected, &mut model);
    assert!(asked, "{lines:?}");
    let mut console = Vec::new();
    assert!(finish_u5(
        &view,
        Path::new(DEVICE),
        &mut Cryptsetup(shared.clone()),
        &mut |line| console.push(line.to_owned())
    ));
    assert_protected(&shared.borrow().disk, Shape::Fido2, 2, (0, 0));
    // Run again, it finds no U5 and writes nothing.
    let commands = shared.borrow().commands;
    assert!(!finish_u5(
        &shared.borrow().disk.view(),
        Path::new(DEVICE),
        &mut Cryptsetup(shared.clone()),
        &mut |_| {}
    ));
    assert_eq!(shared.borrow().commands, commands);
}

/// End of input at each prompt before the cap halts the boot before any
/// command: the recovery key's, the cancellation's, a pending key's and
/// the new PIN's.
#[test]
fn end_of_input_halts() {
    for eof in [
        RECOVERY_PROMPT,
        CANCEL_PROMPT,
        "td new security key",
        protected_release::NEW_PIN_PROMPT,
    ] {
        let log = Log::default();
        let shared = world(Disk::u0(Shape::Tpm), &log);
        let mut model = Model::new(&log);
        let mut person = Person::new();
        if eof == CANCEL_PROMPT {
            person.recovery = vec![b""];
        }
        person.eof = Some(eof);
        let booted = boot_once(
            &shared,
            Some(&mut model),
            &mut security_keys(&log),
            &mut person,
            true,
        );
        assert!(matches!(booted.outcome, Outcome::Halt(_)), "{eof}");
        assert!(person.asked.last().is_some_and(|p| p.starts_with(eof)));
        assert_eq!(shared.borrow().commands, 0, "{eof}");
        assert!(!log.borrow().iter().any(|e| e == "tpm seal"), "{eof}");
    }
    // After the cap, at the recovery key's prompt again.
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Fido2), &log);
    let mut person = Person::new();
    let other: &'static [u8] = Box::leak(
        crate::recovery::RecoveryKey::generate()
            .unwrap()
            .passphrase()
            .expose()
            .to_vec()
            .into_boxed_slice(),
    );
    person.recovery = vec![other];
    person.eof = Some(RECOVERY_AGAIN_PROMPT);
    let booted = boot_once(&shared, None, &mut security_keys(&log), &mut person, true);
    assert!(matches!(booted.outcome, Outcome::Halt(_)));
    assert_eq!(
        upgrade::check(&shared.borrow().disk.view()).map(|(s, _)| s),
        Ok(State::U0)
    );
}

/// A wrong recovery key spends no round: more of them than the rounds a
/// boot runs still end in the upgrade.
#[test]
fn wrong_recovery_keys_ask_again_without_limit() {
    let other: &'static [u8] = Box::leak(
        crate::recovery::RecoveryKey::generate()
            .unwrap()
            .passphrase()
            .expose()
            .to_vec()
            .into_boxed_slice(),
    );
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Fido2), &log);
    let mut person = Person::new();
    person.recovery = vec![other; MAX_ROUNDS + 6];
    let booted = boot_once(&shared, None, &mut security_keys(&log), &mut person, true);
    assert!(
        matches!(booted.outcome, Outcome::Upgraded { .. }),
        "{:?}",
        booted.console.last()
    );
    assert_protected(&shared.borrow().disk, Shape::Fido2, 2, (0, 0));
}

/// An upgrading header `check` refuses, here for a foreign token that
/// joined it in U0, still offers U0's cancellation: exactly `cancel`
/// cancels, and anything else halts, before any command.
#[test]
fn a_refused_u0_header_still_offers_the_cancellation() {
    for (answer, cancels) in [(&b"cancel"[..], true), (&b"go on"[..], false)] {
        let log = Log::default();
        let mut disk = Disk::u0(Shape::Tpm);
        disk.tokens.insert(9, Tok::Foreign(vec![0]));
        let shared = world(disk, &log);
        let mut person = Person::new();
        person.cancel = vec![answer];
        let booted = boot_once(&shared, None, &mut security_keys(&log), &mut person, true);
        assert_eq!(person.asked, [CANCEL_PROMPT]);
        assert_eq!(matches!(booted.outcome, Outcome::Cancel), cancels);
        assert_eq!(matches!(booted.outcome, Outcome::Halt(_)), !cancels);
        assert!(booted.console[0].contains("the upgrade cannot run"));
        assert_eq!(shared.borrow().commands, 0);
    }
    // One `cancel` refuses too, past U0: halted unasked.
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Tpm), &log);
    let mut model = Model::new(&log);
    shared.borrow_mut().cut = Some(4);
    boot_once(
        &shared,
        Some(&mut model),
        &mut security_keys(&log),
        &mut Person::new(),
        true,
    );
    let mut world = shared.borrow_mut();
    world.cut = None;
    world.opened = None;
    world.disk.tokens.insert(9, Tok::Foreign(vec![]));
    drop(world);
    assert!(upgrade::cancel(&shared.borrow().disk.view()).is_err());
    let mut person = Person::new();
    let booted = boot_once(&shared, None, &mut security_keys(&log), &mut person, true);
    assert!(matches!(booted.outcome, Outcome::Halt(_)));
    assert!(person.asked.is_empty());
}

/// A drop, then a primary that changes after the seal in the same boot:
/// U4 is planned again without the dropped token and with the new
/// protector removed, and the boot completes.
#[test]
fn a_drop_and_a_changed_primary_complete_in_one_boot() {
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Tpm), &log);
    let mut model = Model::new(&log);
    let state = model.state.clone();
    shared.borrow_mut().after_seal = Some(Box::new(move || {
        state.borrow_mut().primary = [0x0c; 34];
    }));
    let mut person = Person::new();
    person.keys = vec![b"drop"];
    let booted = boot_once(
        &shared,
        Some(&mut model),
        &mut security_keys(&log),
        &mut person,
        true,
    );
    assert!(
        matches!(booted.outcome, Outcome::Upgraded { .. }),
        "{:?}",
        booted.console
    );
    assert!(booted
        .console
        .iter()
        .any(|line| line.contains("storage primary is not the one")));
    assert_protected(&shared.borrow().disk, Shape::Tpm, 1, (0, 0));
    assert!(!log.borrow().iter().any(|e| e.starts_with("tpm lockout")));
}

/// A take refused before its token's import, here for a lockout
/// authorization set after the pre-cap read, ends U4 with the TPM PIN
/// protector and no lockout token, the console saying so.
#[test]
fn a_take_refused_before_its_import_keeps_the_protector_alone() {
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Tpm), &log);
    let mut model = Model::new(&log);
    let state = model.state.clone();
    shared.borrow_mut().after_seal = Some(Box::new(move || {
        state.borrow_mut().da.lockout_auth_set = true;
    }));
    let booted = boot_once(
        &shared,
        Some(&mut model),
        &mut security_keys(&log),
        &mut Person::new(),
        true,
    );
    assert!(
        matches!(booted.outcome, Outcome::Upgraded { .. }),
        "{:?}",
        booted.console
    );
    assert!(booted
        .console
        .iter()
        .any(|line| line.starts_with("td: no lockout take (it did not complete)")));
    assert!(!booted
        .console
        .iter()
        .any(|line| line.starts_with("td: the TPM is unusable")));
    assert_protected(&shared.borrow().disk, Shape::Tpm, 2, (1, 0));
}

/// A take that fails after its token's import keeps the TPM PIN
/// protector and the lockout token, the console saying so: the header
/// reads U5, which the boot then finishes.
#[test]
fn a_take_failing_after_its_import_keeps_the_protector() {
    let log = Log::default();
    let shared = world(Disk::u0(Shape::Tpm), &log);
    let mut model = Model::new(&log);
    model.state.borrow_mut().refuse_parameters = true;
    let booted = boot_once(
        &shared,
        Some(&mut model),
        &mut security_keys(&log),
        &mut Person::new(),
        true,
    );
    assert!(
        matches!(booted.outcome, Outcome::Upgraded { .. }),
        "{:?}",
        booted.console
    );
    assert!(booted
        .console
        .iter()
        .any(|line| line.starts_with("td: the lockout token is imported")));
    assert!(!booted
        .console
        .iter()
        .any(|line| line.starts_with("td: the TPM is unusable")));
    assert_protected(&shared.borrow().disk, Shape::Tpm, 2, (1, 1));
}
