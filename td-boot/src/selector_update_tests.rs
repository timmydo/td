//! The pre-seal and the commit over an ESP the test builds with td's own
//! FAT32 writer, a header the scripted cryptsetup rewrites, and a
//! `current` deployment and selector the test names
//! (td-install/ENCRYPTION.md item 8, 8e's "Commit tests").
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::rc::Rc;
use td_protector::luks2::HeaderCopy;
use td_protector::pin::{PinPcrs, PRIMARY_NAME_LEN, SALT_LEN};
use td_protector::protected::{Token, UpdateRetry};
use td_protector::protected_release::Entry;
use td_tpm::SealedObject;

const BASE: u64 = 1 << 20;
const SECTORS: u64 = 67_000;
const UUID: &str = "0f0e0d0c-0b0a-4908-8706-050403020100";
const VOLUME_KEY: [u8; 64] = [0x77; 64];
const DEVICE: &str = "/dev/vda2";
const TEMPLATE: &[u8] = b"070701 the stock selector template";
const ARCHIVE: &[u8] = b"070701 this selector's own key, identity and policy";

/// The disk in memory, shared with whoever reopens the ESP, and how many
/// more writes it takes before each one fails.
#[derive(Clone)]
struct Disk(Rc<RefCell<Vec<u8>>>, Rc<Cell<Option<usize>>>);

impl td_fat::Device for Disk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let bytes = self.0.borrow();
        let at = usize::try_from(offset).unwrap();
        let slice = bytes
            .get(at..at + buf.len())
            .ok_or_else(|| io::Error::other("read past the disk"))?;
        buf.copy_from_slice(slice);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        match self.1.get() {
            Some(0) => return Err(io::Error::other("the disk failed")),
            Some(left) => self.1.set(Some(left - 1)),
            None => {}
        }
        let at = usize::try_from(offset).unwrap();
        self.0.borrow_mut()[at..at + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A kernel: the Authenticode test image, `variant` changing a section.
fn kernel(variant: u8) -> Vec<u8> {
    let mut image = authenticode::tests::image();
    image[0x300] = variant;
    image
}

/// A prepared selector: `template`, its padding, then `archive`.
fn prepared(template: &[u8], archive: &[u8]) -> Vec<u8> {
    let mut bytes = template.to_vec();
    bytes.resize(
        template.len() + selector_appendix::padding(template.len() as u64).unwrap(),
        0,
    );
    bytes.extend_from_slice(archive);
    bytes
}

/// An ESP holding `running` as `BOOTX64.EFI` and `INITRD`, and `staged`
/// staged through td-fat as the storage worker stages it.
fn esp_disk(running: (&[u8], &[u8]), staged: Option<(&[u8], &[u8])>) -> Disk {
    use crate::fat::{Node, Volume};
    let volume = Volume {
        bytes_per_sector: 512,
        total_sectors: SECTORS,
        hidden_sectors: (BASE / 512) as u32,
        volume_id: 0x7d1e_9a30,
        label: "TD-ESP".into(),
        sectors_per_cluster: Some(1),
        root: vec![(
            "EFI".into(),
            Node::Dir(vec![(
                "BOOT".into(),
                Node::Dir(vec![
                    ("BOOTX64.EFI".into(), Node::File(running.0)),
                    ("INITRD".into(), Node::File(running.1)),
                ]),
            )]),
        )],
    };
    let image = crate::fat::build(&volume).unwrap();
    let mut bytes = vec![0u8; usize::try_from(BASE + SECTORS * 512).unwrap()];
    for extent in &image.extents {
        let at = usize::try_from(BASE + extent.offset).unwrap();
        bytes[at..at + extent.bytes.len()].copy_from_slice(&extent.bytes);
    }
    let disk = Disk(Rc::new(RefCell::new(bytes)), Rc::new(Cell::new(None)));
    if let Some((k, i)) = staged {
        let mut esp = Esp::open(disk.clone(), BASE, SECTORS * 512).unwrap();
        let (mut k, mut i) = (k, i);
        esp.stage(
            td_fat::Input {
                len: k.len() as u64,
                reader: &mut k,
            },
            td_fat::Input {
                len: i.len() as u64,
                reader: &mut i,
            },
        )
        .unwrap();
    }
    disk
}

/// The pair `EFI/BOOT` names now, and whether a staged pair is beside it.
fn pair_on(disk: &Disk) -> (Vec<u8>, Vec<u8>, bool) {
    let esp = Esp::open(disk.clone(), BASE, SECTORS * 512).unwrap();
    let boot = esp.boot().clone();
    (
        read_file(&esp, &boot.pair.kernel).unwrap(),
        read_file(&esp, &boot.pair.initrd).unwrap(),
        boot.staged.is_some(),
    )
}

/// td's proven lockout token for `primary` under the volume key.
fn lockout(primary: [u8; PRIMARY_NAME_LEN]) -> td_protector::protected::Lockout {
    let auth = td_tpm::LockoutAuth::from_bytes(&mut [0x42; 32]).unwrap();
    let uuid = td_protector::lockout::volume_uuid(UUID).unwrap();
    let mut token = td_protector::lockout::seal(&VOLUME_KEY, &uuid, &auth, &primary).unwrap();
    token.proven = true;
    token
}

fn tpm_pin(keyslot: Option<u8>, salt: u8) -> TpmPin {
    TpmPin {
        keyslot,
        pcrs: PinPcrs::Selector,
        primary: [0x0b; PRIMARY_NAME_LEN],
        salt: [salt; SALT_LEN],
        sealed: SealedObject {
            public: vec![salt; 48],
            private: vec![salt; 96],
        },
    }
}

/// The volume's keyslots and tokens, which the header reader and the
/// scripted cryptsetup share.
#[derive(Default)]
struct Volume {
    keyslots: Vec<u8>,
    tokens: BTreeMap<u8, Token>,
    log: Vec<String>,
    /// cryptsetup fails every token removal.
    refuse_removal: bool,
}

impl Volume {
    /// The TPM-primary shape: the recovery key in keyslot 0 and its
    /// marker as token 0, the tpm-pin protector in keyslot 1 as token 1,
    /// and td's proven lockout, anchoring that token's primary under the
    /// volume key, as token 2.
    fn tpm() -> Self {
        Self {
            keyslots: vec![0, 1],
            tokens: BTreeMap::from([
                (0, Token::RecoveryKey { keyslot: Some(0) }),
                (1, Token::TpmPin(tpm_pin(Some(1), 1))),
                (2, Token::Lockout(lockout([0x0b; PRIMARY_NAME_LEN]))),
            ]),
            ..Self::default()
        }
    }

    fn header(&self) -> Header {
        Header {
            copy: HeaderCopy::Primary,
            seqid: 1,
            hdr_size: 16384,
            uuid: UUID.into(),
            label: "td-system".into(),
            keyslots: self.keyslots.clone(),
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
            .iter()
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

    fn retry(&self) -> Option<UpdateRetry> {
        self.tokens.values().find_map(|token| match token {
            Token::UpdateRetry(retry) => Some(retry.clone()),
            _ => None,
        })
    }

    fn pins(&self) -> Vec<(u8, Option<u8>)> {
        self.tokens
            .iter()
            .filter_map(|(number, token)| match token {
                Token::TpmPin(pin) => Some((*number, pin.keyslot)),
                _ => None,
            })
            .collect()
    }
}

/// cryptsetup's token commands over `Volume`.
struct Cryptsetup(Rc<RefCell<Volume>>);

impl Runner for Cryptsetup {
    fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
        let words: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(words.last().map(String::as_str), Some(DEVICE));
        let id = || -> Option<u8> {
            let at = words.iter().position(|word| word == "--token-id")?;
            Some(words[at + 1].parse().unwrap())
        };
        let mut volume = self.0.borrow_mut();
        match (words[0].as_str(), words[1].as_str()) {
            ("token", "import") => {
                let token = Token::decode(input).unwrap();
                let replace = words.iter().any(|word| word == "--token-replace");
                let number = match id() {
                    Some(id) => {
                        assert_eq!(volume.tokens.contains_key(&id), replace, "{words:?}");
                        id
                    }
                    None => (0..32).find(|n| !volume.tokens.contains_key(n)).unwrap(),
                };
                let kind = token.kind().1.unwrap_or("?");
                volume.log.push(format!("import {number} {kind}"));
                volume.tokens.insert(number, token);
            }
            ("token", "remove") => {
                if volume.refuse_removal {
                    return Err(io::Error::other("cryptsetup refused the removal"));
                }
                let id = id().unwrap();
                assert!(volume.tokens.remove(&id).is_some());
                volume.log.push(format!("remove {id}"));
            }
            _ => panic!("unexpected cryptsetup {words:?}"),
        }
        Ok(())
    }

    fn metadata(&mut self, device: &Path) -> io::Result<Vec<u8>> {
        assert_eq!(device, Path::new(DEVICE));
        Ok(self.0.borrow().dump())
    }
}

/// The selector's machine, scripted.
struct Fake {
    disk: Disk,
    volume: Rc<RefCell<Volume>>,
    prediction: Result<Pcrs, String>,
    /// What the pre-seal asked to predict: the running and staged chains.
    asked: Vec<(Chain, Chain)>,
    current: Result<(Vec<u8>, Vec<u8>), Unverified>,
    appendix: Vec<u8>,
    /// Run on the disk as the commit begins, after the pre-seal's read.
    meddle: Option<fn(&Disk)>,
}

impl Fake {
    /// `current` boots `kernel(1)` with `TEMPLATE`; the ESP runs
    /// `kernel(0)` and stages `staged`.
    fn new(staged: Option<(&[u8], &[u8])>) -> Self {
        Self {
            disk: esp_disk((&kernel(0), b"the running selector"), staged),
            volume: Rc::new(RefCell::new(Volume::tpm())),
            prediction: Ok(pcrs()),
            asked: Vec::new(),
            current: Ok((kernel(1), TEMPLATE.to_vec())),
            appendix: ARCHIVE.to_vec(),
            meddle: None,
        }
    }

    /// The authentic staged pair: current's kernel and its template with
    /// this selector's archive.
    fn authentic() -> Self {
        Self::new(Some((&kernel(1), &prepared(TEMPLATE, ARCHIVE))))
    }

    fn cryptsetup(&self) -> Cryptsetup {
        Cryptsetup(self.volume.clone())
    }
}

fn pcrs() -> Pcrs {
    Pcrs {
        pcr4: [4; 32],
        pcr7: [7; 32],
        pcr9: [9; 32],
    }
}

impl Machine for Fake {
    type Disk = Disk;
    fn esp(&mut self) -> Result<Esp<Disk>, String> {
        Esp::open(self.disk.clone(), BASE, SECTORS * 512).map_err(|error| error.to_string())
    }
    fn predict(&mut self, running: &Chain, staged: &Chain) -> Result<Pcrs, String> {
        self.asked.push((*running, *staged));
        self.prediction.clone()
    }
    fn header(&mut self) -> Result<Header, String> {
        Ok(self.volume.borrow().header())
    }
    fn current(&mut self, root: &Path) -> Result<Current, Unverified> {
        assert_eq!(root, Path::new("/volume"));
        if let Some(meddle) = self.meddle.take() {
            meddle(&self.disk);
        }
        match &self.current {
            Ok((kernel, template)) => Ok(Current {
                kernel: digest(kernel),
                template: template.clone(),
            }),
            Err(Unverified::Refused(why)) => Err(Unverified::Refused(why.clone())),
            Err(Unverified::Unavailable(why)) => Err(Unverified::Unavailable(why.clone())),
        }
    }
    fn appendix(&mut self) -> Result<Vec<u8>, String> {
        Ok(self.appendix.clone())
    }
}

/// The `update` question's answers, in order.
struct Answers(Vec<&'static [u8]>, usize);

impl Prompt for Answers {
    fn read(&mut self, prompt: &str) -> Result<Entered, String> {
        assert_eq!(prompt, UPDATE_PROMPT);
        self.1 += 1;
        Ok(match self.0.pop() {
            Some(answer) => Entered::Line(Entry::new(answer)),
            None => Entered::Eof,
        })
    }
}

/// One boot's pre-seal and, with the predicted object `finish` hands on
/// when the prediction held, its commit; what the console said.
fn boot(fake: &mut Fake, answers: &mut Answers) -> (Outcome, Vec<String>) {
    boot_with(fake, answers, true)
}

/// `boot`, where `kept` says whether a held prediction's object was
/// sealed and kept to the commit.
fn boot_with(fake: &mut Fake, answers: &mut Answers, kept: bool) -> (Outcome, Vec<String>) {
    let mut lines = Vec::new();
    let mut console = |line: &str| lines.push(line.to_string());
    let staged = preseal(fake, &mut console).expect("a staged update");
    let predicted = staged
        .predicted()
        .filter(|_| kept)
        .map(|_| tpm_pin(Some(1), 0x2d));
    let mut cryptsetup = fake.cryptsetup();
    let committed = commit(
        staged,
        predicted,
        Path::new("/volume"),
        fake,
        &mut cryptsetup,
        Path::new(DEVICE),
        &VOLUME_KEY,
        answers,
        &mut console,
    );
    (committed, lines)
}

fn no_answers() -> Answers {
    Answers(Vec::new(), 0)
}

/// The pre-seal reads each staged file once for its digests, and asks
/// for the chain of the running pair with the staged pair's in its
/// place; no staged pair reads nothing.
#[test]
fn the_preseal_reads_the_staged_pair_and_predicts_its_chain() {
    let initrd = prepared(TEMPLATE, ARCHIVE);
    let mut fake = Fake::authentic();
    let mut lines = Vec::new();
    let staged = preseal(&mut fake, &mut |line| lines.push(line.to_string())).unwrap();
    assert_eq!(
        staged.digests,
        Digests {
            kernel: digest(&kernel(1)),
            authenticode: authenticode::sha256(&kernel(1)).unwrap(),
            initrd: digest(&initrd),
        }
    );
    assert_eq!(
        fake.asked,
        [(
            Chain {
                application: authenticode::sha256(&kernel(0)).unwrap(),
                initrd: digest(b"the running selector"),
            },
            Chain {
                application: authenticode::sha256(&kernel(1)).unwrap(),
                initrd: digest(&initrd),
            }
        )]
    );
    assert_eq!(
        staged.predicted(),
        Some(Predicted {
            pcr4: [4; 32],
            pcr7: [7; 32],
            pcr9: [9; 32],
        })
    );
    assert!(staged.read.staged.is_some());

    let mut fake = Fake::authentic();
    fake.prediction = Err("a boot option returned".into());
    let mut lines = Vec::new();
    let staged = preseal(&mut fake, &mut |line| lines.push(line.to_string())).unwrap();
    assert_eq!(staged.predicted(), None);
    assert!(lines
        .iter()
        .any(|line| line.ends_with("boot chain: a boot option returned")));

    let mut fake = Fake::new(None);
    let mut lines = Vec::new();
    assert!(preseal(&mut fake, &mut |line| lines.push(line.to_string())).is_none());
    assert!(fake.asked.is_empty() && lines.is_empty());
}

/// An authentic pair with its predicted object: the object is imported as
/// a second tpm-pin token naming the keyslot that released, then the
/// staged pair becomes `EFI/BOOT`'s, and an earlier retry token goes.
#[test]
fn an_authentic_pair_imports_the_prediction_then_commits() {
    let initrd = prepared(TEMPLATE, ARCHIVE);
    let mut fake = Fake::authentic();
    fake.volume.borrow_mut().tokens.insert(
        5,
        Token::UpdateRetry(UpdateRetry {
            pair: [1; 32],
            tag: [2; 32],
        }),
    );
    let (committed, lines) = boot(&mut fake, &mut no_answers());
    assert_eq!(committed, Outcome::Committed, "{lines:?}");
    let volume = fake.volume.borrow();
    assert_eq!(volume.log, ["import 3 tpm-pin", "remove 5"]);
    assert_eq!(volume.pins(), [(1, Some(1)), (3, Some(1))]);
    assert_eq!(volume.retry(), None);
    assert_eq!(pair_on(&fake.disk), (kernel(1), initrd, false));
}

/// ENCRYPTION.md 8e's commit tests: another template with this selector's
/// exact archive, another kernel, and a pair from `previous` each delete
/// the staged update after the cap with no object imported; another
/// archive is refused as another template is.
#[test]
fn a_pair_that_is_not_currents_is_deleted_and_nothing_imported() {
    let cases: [(&str, Vec<u8>, Vec<u8>); 4] = [
        (
            "another template",
            kernel(1),
            prepared(b"070701 another template", ARCHIVE),
        ),
        (
            "another archive",
            kernel(1),
            prepared(TEMPLATE, b"070701 another selector's key"),
        ),
        ("another kernel", kernel(2), prepared(TEMPLATE, ARCHIVE)),
        (
            "previous",
            kernel(3),
            prepared(b"070701 previous's template", ARCHIVE),
        ),
    ];
    for (name, k, i) in cases {
        let mut fake = Fake::new(Some((&k, &i)));
        fake.volume.borrow_mut().tokens.insert(
            5,
            Token::UpdateRetry(UpdateRetry {
                pair: [1; 32],
                tag: [2; 32],
            }),
        );
        let (committed, lines) = boot(&mut fake, &mut no_answers());
        assert_eq!(committed, Outcome::Deleted, "{name}: {lines:?}");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("the staged selector update is refused")),
            "{name}"
        );
        let volume = fake.volume.borrow();
        assert_eq!(volume.log, ["remove 5"], "{name}");
        assert_eq!(volume.pins(), [(1, Some(1))], "{name}");
        assert_eq!(
            pair_on(&fake.disk),
            (kernel(0), b"the running selector".to_vec(), false),
            "{name}"
        );
    }
    // A deployment that is not current, by td-boot's own reading of the
    // volume, is refused as well.
    let mut fake = Fake::authentic();
    fake.current = Err(Unverified::Refused("no current deployment".into()));
    let (committed, _) = boot(&mut fake, &mut no_answers());
    assert_eq!(committed, Outcome::Deleted);
    assert!(!pair_on(&fake.disk).2);
}

/// `current` that cannot be read leaves everything as it was.
#[test]
fn an_unreadable_current_keeps_the_staged_update_and_writes_nothing() {
    let mut fake = Fake::authentic();
    fake.current = Err(Unverified::Unavailable("the loop device".into()));
    let (committed, _) = boot(&mut fake, &mut no_answers());
    assert_eq!(committed, Outcome::Kept);
    assert!(fake.volume.borrow().log.is_empty());
    assert!(pair_on(&fake.disk).2);
}

/// A prediction failure with an authentic pair keeps it and writes the
/// retry token; only the retry's failure with that token verifying offers
/// `update`, which commits and restarts, and any other answer deletes the
/// staged update. Both remove the token.
#[test]
fn a_failed_prediction_keeps_the_pair_and_only_its_retry_offers_update() {
    let initrd = prepared(TEMPLATE, ARCHIVE);
    let mut fake = Fake::authentic();
    fake.prediction = Err("a boot option returned".into());
    let mut answers = no_answers();
    let (committed, lines) = boot(&mut fake, &mut answers);
    assert_eq!(committed, Outcome::Kept, "{lines:?}");
    assert_eq!(answers.1, 0);
    assert!(!lines.iter().any(|line| line == UPDATE_WARNING));
    let retry = fake.volume.borrow().retry().unwrap();
    let uuid = td_protector::lockout::volume_uuid(UUID).unwrap();
    assert!(update_retry::verifies(
        &VOLUME_KEY,
        &uuid,
        &retry,
        &digest(&kernel(1)),
        &digest(&initrd)
    ));
    assert_eq!(fake.volume.borrow().log, ["import 3 update-retry"]);
    assert!(pair_on(&fake.disk).2);

    // The retry fails again: `update` commits and restarts.
    let mut answers = Answers(vec![b"update"], 0);
    let (committed, lines) = boot(&mut fake, &mut answers);
    assert_eq!(committed, Outcome::Restart, "{lines:?}");
    assert_eq!(answers.1, 1);
    assert!(lines.iter().any(|line| line == UPDATE_WARNING));
    assert_eq!(fake.volume.borrow().retry(), None);
    assert_eq!(fake.volume.borrow().pins(), [(1, Some(1))]);
    assert_eq!(pair_on(&fake.disk), (kernel(1), initrd.clone(), false));

    // Any other answer, end of input included, deletes it.
    for answer in [&b"Update"[..], b"yes", b"update "] {
        let mut fake = Fake::authentic();
        fake.prediction = Err("a boot option returned".into());
        boot(&mut fake, &mut no_answers());
        let mut answers = Answers(vec![answer], 0);
        let (committed, _) = boot(&mut fake, &mut answers);
        assert_eq!(committed, Outcome::Deleted);
        assert_eq!(fake.volume.borrow().retry(), None);
        assert_eq!(
            pair_on(&fake.disk),
            (kernel(0), b"the running selector".to_vec(), false)
        );
    }
    let mut fake = Fake::authentic();
    fake.prediction = Err("a boot option returned".into());
    boot(&mut fake, &mut no_answers());
    let (committed, _) = boot(&mut fake, &mut no_answers());
    assert_eq!(committed, Outcome::Deleted);
}

/// A retry token forged, copied from another volume, or naming another
/// pair offers nothing: the token is rewritten for this pair and the
/// update waits.
#[test]
fn a_retry_token_that_does_not_verify_offers_nothing() {
    let initrd = prepared(TEMPLATE, ARCHIVE);
    let uuid = td_protector::lockout::volume_uuid(UUID).unwrap();
    let other_uuid =
        td_protector::lockout::volume_uuid("1f0e0d0c-0b0a-4908-8706-050403020100").unwrap();
    let ours =
        update_retry::token(&VOLUME_KEY, &uuid, &digest(&kernel(1)), &digest(&initrd)).unwrap();
    let mut forged = ours.clone();
    forged.tag[0] ^= 1;
    let tokens = [
        ("forged", forged),
        (
            "another volume's key",
            update_retry::token(&[0x55; 64], &uuid, &digest(&kernel(1)), &digest(&initrd)).unwrap(),
        ),
        (
            "another volume's UUID",
            update_retry::token(
                &VOLUME_KEY,
                &other_uuid,
                &digest(&kernel(1)),
                &digest(&initrd),
            )
            .unwrap(),
        ),
        (
            "another pair",
            update_retry::token(&VOLUME_KEY, &uuid, &digest(&kernel(2)), &digest(&initrd)).unwrap(),
        ),
    ];
    for (name, token) in tokens {
        let mut fake = Fake::authentic();
        fake.prediction = Err("a boot option returned".into());
        fake.volume
            .borrow_mut()
            .tokens
            .insert(4, Token::UpdateRetry(token));
        let mut answers = Answers(vec![b"update"], 0);
        let (committed, lines) = boot(&mut fake, &mut answers);
        assert_eq!(committed, Outcome::Kept, "{name}");
        assert_eq!(answers.1, 0, "{name}");
        assert!(!lines.iter().any(|line| line == UPDATE_WARNING), "{name}");
        assert_eq!(fake.volume.borrow().retry(), Some(ours.clone()), "{name}");
        assert_eq!(
            fake.volume.borrow().log,
            ["import 4 update-retry"],
            "{name}"
        );
        assert!(pair_on(&fake.disk).2, "{name}");
    }
}

/// The predicted object is imported only beside the one tpm-pin token
/// naming the keyslot that released: a header holding another, as a cut
/// commit leaves it, keeps the staged update for the next PIN boot, whose
/// plan retires the extra token.
#[test]
fn an_import_the_header_refuses_keeps_the_staged_update() {
    let mut fake = Fake::authentic();
    fake.volume
        .borrow_mut()
        .tokens
        .insert(3, Token::TpmPin(tpm_pin(Some(1), 3)));
    let (committed, lines) = boot(&mut fake, &mut no_answers());
    assert_eq!(committed, Outcome::Kept, "{lines:?}");
    assert!(fake.volume.borrow().log.is_empty());
    assert!(pair_on(&fake.disk).2);
}

#[test]
fn a_manifest_digest_is_exactly_64_lowercase_digits() {
    let digest = "ab".repeat(32);
    assert_eq!(parse_digest(&digest), Some([0xab; 32]));
    for bad in [
        "AB".repeat(32),
        "ab".repeat(31),
        format!("{digest}0"),
        "g".repeat(64),
    ] {
        assert_eq!(parse_digest(&bad), None, "{bad}");
    }
}

#[test]
fn the_device_number_splits_as_glibc_makedev_does() {
    assert_eq!(major_minor(0x0000_0000_0000_fd02), (0xfd, 0x02));
    assert_eq!(major_minor(0x1_0001_0300), (0x103, 0x10_0000));
}

/// The prediction held this boot but no object reached the commit (a seal
/// the TPM refused, or one `finish` discarded): the staged update and an
/// earlier retry token stay as they are, and `update` is not offered,
/// since its warning says the prediction failed.
#[test]
fn a_held_prediction_without_its_object_keeps_everything_and_offers_nothing() {
    let mut fake = Fake::authentic();
    fake.prediction = Err("a boot option returned".into());
    boot(&mut fake, &mut no_answers());
    let retry = fake.volume.borrow().retry().unwrap();
    fake.prediction = Ok(pcrs());
    let mut answers = Answers(vec![b"update"], 0);
    let (outcome, lines) = boot_with(&mut fake, &mut answers, false);
    assert_eq!(outcome, Outcome::Kept, "{lines:?}");
    assert_eq!(answers.1, 0);
    assert!(!lines.iter().any(|line| line == UPDATE_WARNING));
    assert_eq!(fake.volume.borrow().retry(), Some(retry));
    assert_eq!(fake.volume.borrow().log, ["import 3 update-retry"]);
    assert!(pair_on(&fake.disk).2);
}

/// A predicted object whose primary no lockout token on the header
/// anchors under the volume key is not imported, and the update waits.
#[test]
fn an_unanchored_prediction_is_not_imported() {
    for anchor in [None, Some([0x0c; PRIMARY_NAME_LEN])] {
        let mut fake = Fake::authentic();
        match anchor {
            None => fake.volume.borrow_mut().tokens.remove(&2),
            Some(primary) => fake
                .volume
                .borrow_mut()
                .tokens
                .insert(2, Token::Lockout(lockout(primary))),
        };
        let (outcome, lines) = boot(&mut fake, &mut no_answers());
        assert_eq!(outcome, Outcome::Kept, "{lines:?}");
        assert!(fake.volume.borrow().log.is_empty());
        assert!(pair_on(&fake.disk).2);
    }
}

/// The retry token goes before the directory commit, and a removal that
/// fails keeps the staged update uncommitted, on either path; a deletion
/// deletes whatever the removal did.
#[test]
fn a_retry_token_that_cannot_be_removed_holds_the_commit() {
    let mut fake = Fake::authentic();
    fake.volume.borrow_mut().tokens.insert(
        5,
        Token::UpdateRetry(UpdateRetry {
            pair: [1; 32],
            tag: [2; 32],
        }),
    );
    fake.volume.borrow_mut().refuse_removal = true;
    let (outcome, _) = boot(&mut fake, &mut no_answers());
    assert_eq!(outcome, Outcome::Kept);
    assert!(pair_on(&fake.disk).2);

    let mut fake = Fake::authentic();
    fake.prediction = Err("a boot option returned".into());
    boot(&mut fake, &mut no_answers());
    fake.volume.borrow_mut().refuse_removal = true;
    let mut answers = Answers(vec![b"update"], 0);
    let (outcome, _) = boot(&mut fake, &mut answers);
    assert_eq!(outcome, Outcome::Kept);
    assert!(fake.volume.borrow().retry().is_some());
    assert!(pair_on(&fake.disk).2);

    let mut answers = Answers(vec![b"no"], 0);
    let (outcome, _) = boot(&mut fake, &mut answers);
    assert_eq!(outcome, Outcome::Deleted);
    assert!(!pair_on(&fake.disk).2);
}

/// A directory write that fails before anything is written keeps the
/// update; one that fails once the sector is synced, freeing the old
/// chains, has committed it, so `update`'s restart still follows, and so
/// does one whose sector write may or may not have landed.
#[test]
fn the_commit_reports_by_where_its_write_failed() {
    fn changed(disk: &Disk) {
        let mut esp = Esp::open(disk.clone(), BASE, SECTORS * 512).unwrap();
        let read = esp.boot().clone();
        esp.delete_staged(&read).unwrap();
    }
    fn after(disk: &Disk) {
        disk.1.set(Some(1));
    }
    fn uncertain(disk: &Disk) {
        disk.1.set(Some(0));
    }
    let cases: [(fn(&Disk), Outcome, Outcome); 3] = [
        (changed, Outcome::Kept, Outcome::Kept),
        (after, Outcome::Committed, Outcome::Restart),
        (uncertain, Outcome::Committed, Outcome::Restart),
    ];
    for (meddle, predicted, retried) in cases {
        let mut fake = Fake::authentic();
        fake.meddle = Some(meddle);
        let (outcome, lines) = boot(&mut fake, &mut no_answers());
        assert_eq!(outcome, predicted, "{lines:?}");

        let mut fake = Fake::authentic();
        fake.prediction = Err("a boot option returned".into());
        boot(&mut fake, &mut no_answers());
        fake.meddle = Some(meddle);
        let mut answers = Answers(vec![b"update"], 0);
        let (outcome, lines) = boot(&mut fake, &mut answers);
        assert_eq!(outcome, retried, "{lines:?}");
    }
    // After the sector: the new pair is EFI/BOOT's.
    let mut fake = Fake::authentic();
    fake.meddle = Some(after);
    boot(&mut fake, &mut no_answers());
    fake.disk.1.set(None);
    let (k, i, staged) = pair_on(&fake.disk);
    assert_eq!(
        (k, i, staged),
        (kernel(1), prepared(TEMPLATE, ARCHIVE), false)
    );
}
