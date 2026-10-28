//! The protected tier's largest headers written by td-luks must leave
//! at least 1 KiB of the 12 KiB JSON area spare. Set TD_LUKS to the built
//! program's absolute path. Reencryption transitions here measure metadata
//! only; the td-luks QEMU oracle tests actual data rewriting and recovery.
//!
//! ```text
//! cargo test --frozen --manifest-path td-protector/Cargo.toml \
//!   --test header_fit -- --ignored
//! ```
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use td_fs::TempDir;
use td_protector::luks::Resilience;
use td_protector::luks::{self, KeyFile, Luks};
use td_protector::luks2::UpgradeView;
use td_protector::luks2::{self, Kind, BINARY_HEADER_LEN, JSON_AREA_LEN};
use td_protector::pin::PinPcrs;
use td_protector::protected::{
    Fido2, FidoRole, Lockout, Operation, Request, Shape, Token, TpmPin, UpdateRetry,
};
use td_protector::protected_plan;
use td_protector::token::{self as device, Role};
use td_protector::upgrade::{
    self, Gathered, Input, Key, State, Step, Tpm, KEYSLOT_JSON, REENCRYPT_JSON,
};
use td_tpm::SealedObject;

/// What every header must leave of the JSON area.
const SPARE: usize = 1024;
/// The recovery key's passphrase, keyslot 0's key.
const RECOVERY: &[u8] = b"000000000000000000000000000000000000000000000000";
/// The JSON text each header measured, td-luks writing it. The
/// fixtures are fixed and every PBKDF parameter forced, so the sizes are
/// exact.
const PROTECTED_JSON: usize = 10510;
const UPGRADE_U2_JOURNAL_JSON: usize = 9846;
const UPGRADE_U2_CHECKSUM_JSON: usize = 9882;

fn scratch(name: &str) -> TempDir {
    TempDir::new(&format!("td-protector-header-fit-{name}")).unwrap()
}

#[path = "support/luks_program.rs"]
mod luks_program;

fn tool() -> Luks {
    Luks::new(luks_program::path())
}

/// A td-formatted volume on a 20 MiB file: keyslot 0 the recovery key.
fn format(cs: &Luks, dir: &Path) -> PathBuf {
    let image = dir.join("volume");
    File::create(&image).unwrap().set_len(20 << 20).unwrap();
    let args = luks::format_args("0f0e0d0c-0b0a-4908-8706-050403020100", "td-system", &image);
    cs.run(&args, RECOVERY).unwrap();
    image
}

fn add_keyslot(cs: &Luks, image: &Path, slot: u8) {
    let key = KeyFile::new(&[slot; 32]).unwrap();
    cs.run(&luks::add_key_args(image, 0, slot, key.path()), RECOVERY)
        .unwrap();
}

fn import(cs: &Luks, image: &Path, id: u8, json: &str) {
    cs.run(&luks::token_import_args(image, Some(id)), json.as_bytes())
        .unwrap();
}

/// The JSON text of each copy, up to its NUL padding; both must agree.
fn json_len(image: &Path) -> usize {
    let mut bytes = vec![0; 2 * 0x4000];
    File::open(image).unwrap().read_exact(&mut bytes).unwrap();
    let used = |copy: &[u8]| {
        let area = &copy[BINARY_HEADER_LEN..BINARY_HEADER_LEN + JSON_AREA_LEN];
        area.iter().position(|byte| *byte == 0).unwrap()
    };
    let primary = used(&bytes[..0x4000]);
    assert_eq!(primary, used(&bytes[0x4000..]), "the copies differ");
    primary
}

fn tpm_pin(slot: Option<u8>) -> Token {
    Token::TpmPin(TpmPin {
        keyslot: slot,
        pcrs: PinPcrs::SecureBoot,
        primary: [0xff; 34],
        salt: [0xff; 32],
        sealed: SealedObject {
            public: vec![0xff; 128],
            private: vec![0xff; 256],
        },
    })
}

fn fido2(slot: Option<u8>) -> Token {
    Token::Fido2(Fido2 {
        keyslot: slot,
        role: FidoRole::Recovery,
        credential: vec![0xff; 255],
        salt: [0xff; 32],
        x: [0xff; 32],
        y: [0xff; 32],
    })
}

fn lockout() -> Token {
    Token::Lockout(Lockout {
        primary: [0xff; 34],
        nonce: [0xff; 32],
        ciphertext: [0xff; 32],
        tag: [0xff; 32],
        proven: true,
    })
}

fn upgrade() -> Token {
    Token::Upgrade {
        shape: Shape::Fido2,
        from: [0xff; 32],
    }
}

/// The largest request: the longest operation name, its nonce and tag.
fn request() -> Token {
    Token::Request(Request {
        operation: Operation::TpmReenroll,
        nonce: [0xff; 32],
        tag: [0xff; 32],
    })
}

fn hex(len: usize) -> String {
    "ff".repeat(len)
}

/// 8e's update-retry token at its bounds, as the vectors' largest.
fn largest_retry() -> Token {
    Token::UpdateRetry(UpdateRetry {
        pair: [0xff; 32],
        tag: [0xff; 32],
    })
}

/// The tokens increment 8i adds, at the bounds their formats take
/// (td-protector/DESIGN.md, "Protected roles"): the secure-boot token's
/// RSA-2048 key, whose TPM2B_PUBLIC holds 316 bytes and private area 236
/// before its IV, at 320 and 256.
fn later_tokens() -> Vec<String> {
    vec![
        format!(
            r#"{{"type":"td-protector","keyslots":[],"role":"secure-boot","salt":"{}","public":"{}","private":"{}"}}"#,
            hex(32),
            hex(320),
            hex(256)
        ),
        format!(
            r#"{{"type":"td-protector","keyslots":[],"role":"config","tags":["{}","{}"]}}"#,
            hex(32),
            hex(32)
        ),
    ]
}

/// The largest protected state: keyslot 0, four fido2 keyslots and two
/// tpm-pin ones (a PIN change cut between its add and its retirement),
/// every token at its bound, the upgrade's own token and 8e's update-retry
/// token beside them, then 8i's tokens. td's reader reads the header
/// before the last two, which it does not know yet.
#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn the_largest_protected_header_fits() {
    let cs = tool();
    let dir = scratch("protected");
    let image = format(&cs, dir.path());
    for slot in 26..=31 {
        add_keyslot(&cs, &image, slot);
    }
    let mut id = 19;
    let mut next = |token: &Token| {
        import(&cs, &image, id, &token.encode().unwrap());
        id += 1;
    };
    for slot in 26..=29 {
        next(&fido2(Some(slot)));
    }
    next(&tpm_pin(Some(30)));
    next(&tpm_pin(Some(31)));
    next(&Token::RecoveryKey { keyslot: Some(0) });
    next(&lockout());
    next(&upgrade());
    next(&request());
    next(&largest_retry());
    let header = luks2::read(&mut File::open(&image).unwrap()).unwrap();
    assert_eq!(header.kind(), Kind::Upgrading);
    assert_eq!(header.protected.len(), 11);
    assert_eq!(header.keyslots, [0, 26, 27, 28, 29, 30, 31]);
    assert_eq!(header.foreign_keyslots(), [0, 26, 27, 28, 29, 30, 31]);
    let theirs = luks2::metadata(&cs.metadata(&image).unwrap()).unwrap();
    assert_eq!(theirs, header.metadata());
    for json in later_tokens() {
        import(&cs, &image, id, &json);
        id += 1;
    }
    let used = json_len(&image);
    eprintln!("largest protected header: {used} of {JSON_AREA_LEN} bytes");
    assert!(
        used + SPARE <= JSON_AREA_LEN,
        "{used} bytes leave under 1 KiB"
    );
    assert_eq!(used, PROTECTED_JSON);
}

fn reencrypt_init(_cs: &Luks, image: &Path, resilience: Resilience) {
    reencrypt_init_only(_cs, image, 0, resilience);
}

/// U2 as the upgrade reaches it: keyslot 0, the four pending fido2 tokens
/// and the `td-upgrade` token, then td-luks's reencryption keyslot,
/// segments and second digest by library initialization. td's reader
/// reads it as an upgrading header, as td-luks's metadata dump does.
#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn a_u2_header_reads_as_upgrading() {
    let cs = tool();
    let dir = scratch("u2-read");
    let image = format(&cs, dir.path());
    for id in 0..4 {
        import(&cs, &image, id, &fido2(None).encode().unwrap());
    }
    import(&cs, &image, 4, &upgrade().encode().unwrap());
    reencrypt_init(&cs, &image, Resilience::Journal);
    let header = luks2::read(&mut File::open(&image).unwrap()).unwrap();
    assert_eq!(header.kind(), Kind::Upgrading);
    assert_eq!(header.keyslots, [0, 1, 2]);
    assert_eq!(header.protected.len(), 5);
    assert!(header.tokens.is_empty() && header.orphans.is_empty());
    let theirs = luks2::metadata(&cs.metadata(&image).unwrap()).unwrap();
    assert_eq!(theirs, header.metadata());
}

/// The largest U2: besides keyslot 0, the pending fido2 tokens and the
/// `td-upgrade` token, every other token naming no keyslot, more than U2
/// holds, then library initialization, after which td-luks refuses a
/// token import.
fn upgrading_u2(name: &str, resilience: Resilience, expected: usize) {
    let cs = tool();
    let dir = scratch(name);
    let image = format(&cs, dir.path());
    let mut id = 19;
    for _ in 0..4 {
        import(&cs, &image, id, &fido2(None).encode().unwrap());
        id += 1;
    }
    import(&cs, &image, id, &upgrade().encode().unwrap());
    id += 1;
    let mut more: Vec<String> = [
        tpm_pin(None),
        tpm_pin(None),
        Token::RecoveryKey { keyslot: None },
        lockout(),
        request(),
        largest_retry(),
    ]
    .iter()
    .map(|token| token.encode().unwrap())
    .collect();
    more.extend(later_tokens());
    for json in more {
        import(&cs, &image, id, &json);
        id += 1;
    }
    reencrypt_init(&cs, &image, resilience);
    let used = json_len(&image);
    eprintln!("{name} header: {used} of {JSON_AREA_LEN} bytes");
    assert!(
        used + SPARE <= JSON_AREA_LEN,
        "{used} bytes leave under 1 KiB"
    );
    assert_eq!(used, expected);
}

#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn the_largest_u2_header_fits_in_journal_resilience() {
    upgrading_u2("u2-journal", Resilience::Journal, UPGRADE_U2_JOURNAL_JSON);
}

#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn the_largest_u2_header_fits_in_checksum_resilience() {
    upgrading_u2(
        "u2-checksum",
        Resilience::Checksum,
        UPGRADE_U2_CHECKSUM_JSON,
    );
}

/// Initialize metadata with the library; this fixture has no live mapping.
fn reencrypt_init_only(_cs: &Luks, image: &Path, slot: u8, resilience: Resilience) {
    let mut file = File::options().read(true).write(true).open(image).unwrap();
    let mode = match resilience {
        Resilience::Checksum => td_luks::reencrypt::Resilience::Checksum,
        Resilience::Journal => td_luks::reencrypt::Resilience::Journal,
    };
    let (plan, _keys) = td_luks::reencrypt::plan_start(
        &mut file,
        &mut td_luks::keyslot::DevRandom,
        slot,
        RECOVERY,
        mode,
    )
    .unwrap();
    td_luks::reencrypt::commit(&mut file, &mut td_luks::keyslot::DevRandom, &plan).unwrap();
}

/// Advance only the metadata. The disposable image contains no payload;
/// this proves planner/header compatibility, not encrypted data recovery.
fn finish_metadata(_cs: &Luks, image: &Path) {
    use td_luks::{reencrypt, write::Plan};
    let mut file = File::options().read(true).write(true).open(image).unwrap();
    loop {
        let (header, layout) = reencrypt::load(&mut file).unwrap();
        let size = (layout.data_size - layout.progress).min(reencrypt::HOTZONE_SIZE);
        let dirty = reencrypt::mark_hotzone(&header.json, layout.data_size, size).unwrap();
        let json = reencrypt::finish_hotzone(&dirty, layout.data_size).unwrap();
        let done = json.get_path(&["config", "requirements"]).is_none();
        let plan = Plan {
            uuid: header.uuid,
            label: header.label,
            seqid: header.seqid + 1,
            json,
            areas: vec![],
            used: header.used,
        };
        reencrypt::commit(&mut file, &mut td_luks::keyslot::DevRandom, &plan).unwrap();
        if done {
            break;
        }
    }
}

/// The upgrade planner's growth bounds hold for what td-luks writes: a
/// keyslot added at a two-digit number, and the reencryption's
/// initialization in both resilience modes.
#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn the_upgrade_growth_bounds_hold() {
    let cs = tool();
    let dir = scratch("growth-keyslot");
    let image = format(&cs, dir.path());
    let before = json_len(&image);
    add_keyslot(&cs, &image, 31);
    let grown = json_len(&image) - before;
    eprintln!("a keyslot adds {grown} bytes");
    assert!(grown <= KEYSLOT_JSON, "{grown}");
    for resilience in [Resilience::Checksum, Resilience::Journal] {
        let dir = scratch(&format!("growth-{resilience:?}"));
        let image = format(&cs, dir.path());
        let before = json_len(&image);
        reencrypt_init_only(&cs, &image, 0, resilience);
        let grown = json_len(&image) - before;
        eprintln!("{resilience:?} reencryption adds {grown} bytes");
        assert!(grown <= REENCRYPT_JSON, "{grown}");
    }
}

fn read_upgrade(image: &Path) -> UpgradeView {
    luks2::read_upgrade(&mut File::open(image).unwrap()).unwrap()
}

/// The secret each of the upgrade's keys stands for in this test.
fn secret(key: Key) -> Vec<u8> {
    match key {
        Key::Recovery => RECOVERY.to_vec(),
        Key::Fido2 { token } => vec![0xf0 + token; 32],
        Key::TpmPin => vec![0x77; 32],
    }
}

/// One plan step through the runner, the reencryption stopped after its
/// initialization when `interrupt` is set.
fn run_step(cs: &Luks, image: &Path, step: &Step, interrupt: bool) {
    match step {
        Step::Reencrypt { slot, resilience } => {
            reencrypt_init_only(cs, image, *slot, *resilience);
            if !interrupt {
                finish_metadata(cs, image);
            }
        }
        Step::ResumeReencrypt => finish_metadata(cs, image),
        Step::TakeLockout => {
            let lockout = Lockout {
                primary: [0x0b; 34],
                nonce: [1; 32],
                ciphertext: [2; 32],
                tag: [3; 32],
                proven: false,
            };
            let (_, import) = upgrade::lockout_import(&read_upgrade(image), &lockout).unwrap();
            run_step(cs, image, &import, false);
        }
        _ => {
            let key = step
                .new_key()
                .map(|key| KeyFile::new(&secret(key)).unwrap());
            let new_key = key
                .as_ref()
                .map_or(Path::new("/nonexistent"), KeyFile::path);
            let args = step.args(image, "td-selector", new_key).unwrap();
            let input = match step.input() {
                Input::Key(key) => secret(key),
                Input::Token(token) => token.encode().unwrap().into_bytes(),
                Input::Nothing => Vec::new(),
            };
            cs.run(&args, &input).unwrap();
        }
    }
}

/// The largest header `phase_one_fits` admits: a converged device-bound
/// volume whose token is at its bounds, holding the two pending tokens a
/// crashed phase 1 left, retried with two new keys at their largest in the
/// TPM shape. The bound it returns, which covers every later plan, leaves
/// 1 KiB, so a retry is never refused for size; phase 1's own header is
/// within it, and reads as U0.
#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn the_largest_phase_one_header_fits() {
    let cs = tool();
    let dir = scratch("phase-one");
    let image = format(&cs, dir.path());
    add_keyslot(&cs, &image, 31);
    let bound = device::Token::new(
        31,
        Role::DeviceBound,
        SealedObject {
            public: vec![0xff; device::MAX_PUBLIC_BYTES],
            private: vec![0xff; device::MAX_PRIVATE_BYTES],
        },
    )
    .unwrap();
    import(&cs, &image, 0, &bound.encode());
    for id in [1, 2] {
        import(&cs, &image, id, &fido2(None).encode().unwrap());
    }
    let view = read_upgrade(&image);
    assert_eq!(view.header.kind(), Kind::DeviceBound);
    assert_eq!(view.json_len, json_len(&image));
    let fits = upgrade::phase_one_fits(&view, Shape::Tpm, 2).unwrap();
    eprintln!(
        "largest phase 1 header: {} bytes, bounded at {fits} of {JSON_AREA_LEN}",
        view.json_len
    );
    assert!(
        fits + SPARE <= JSON_AREA_LEN,
        "{fits} bytes leave under 1 KiB"
    );
    let Token::Fido2(key) = fido2(None) else {
        panic!("a fido2 token")
    };
    let keys = [key.clone(), key];
    let metadata = cs.metadata(&image).unwrap();
    for step in upgrade::phase_one(&view, Shape::Tpm, &keys)
        .unwrap()
        .confirm(&metadata)
        .unwrap()
    {
        run_step(&cs, &image, &step, false);
    }
    let after = read_upgrade(&image);
    assert_eq!(upgrade::check(&after), Ok((State::U0, Shape::Tpm)));
    assert!(json_len(&image) <= fits);
}

/// The upgrade from U0 to a protected header over the headers td-luks
/// writes: phase 1's pending tokens and `td-upgrade` token over a
/// device-bound volume, then each state's plan, confirmed against
/// td-luks's own metadata and run through the runner: the TPM shape in
/// both resilience modes, uncut and interrupted after the reencryption's
/// initialization (U2), and the FIDO2 shape interrupted so. The result is
/// a header of the shape the protected planner admits, its volume key
/// rotated, every protector opening its keyslot and a wrong key refused.
#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn the_upgrade_plans_run_over_td_luks_metadata() {
    let cs = tool();
    for (shape, resilience, interrupt) in [
        (Shape::Tpm, Resilience::Checksum, false),
        (Shape::Tpm, Resilience::Journal, true),
        (Shape::Fido2, Resilience::Checksum, true),
    ] {
        let case = format!("{shape:?} {resilience:?} {interrupt}");
        let dir = scratch(&format!("upgrade-{shape:?}-{resilience:?}"));
        let image = format(&cs, dir.path());
        add_keyslot(&cs, &image, 1);
        let bound = device::Token::new(
            1,
            Role::DeviceBound,
            SealedObject {
                public: vec![0x00, 0x08, 0x01],
                private: vec![0x01, 0x01],
            },
        )
        .unwrap();
        import(&cs, &image, 0, &bound.encode());
        let role = match shape {
            Shape::Tpm => FidoRole::Recovery,
            Shape::Fido2 => FidoRole::Primary,
        };
        let keys = [1, 2].map(|n| Fido2 {
            keyslot: None,
            role,
            credential: vec![n; 16],
            salt: [n; 32],
            x: [n; 32],
            y: [n; 32],
        });
        // Phase 1 as the storage worker writes it, confirmed against
        // td-luks's own metadata: tokens 1 and 2, then `td-upgrade`.
        let view = read_upgrade(&image);
        let from = view.volume.segment.unwrap();
        let metadata = cs.metadata(&image).unwrap();
        for step in upgrade::phase_one(&view, shape, &keys)
            .unwrap()
            .confirm(&metadata)
            .unwrap()
        {
            run_step(&cs, &image, &step, false);
        }
        assert_eq!(
            upgrade::check(&read_upgrade(&image)),
            Ok((State::U0, shape)),
            "{case}"
        );
        let sealed = TpmPin {
            keyslot: None,
            pcrs: PinPcrs::Selector,
            primary: [0x0b; 34],
            salt: [0x5a; 32],
            sealed: SealedObject {
                public: vec![0x77; 78],
                private: vec![0x66; 158],
            },
        };
        let gathered = Gathered {
            fido2: &[1, 2],
            dropped: &[],
            tpm: match shape {
                Shape::Tpm => Tpm::Sealed(&sealed),
                Shape::Fido2 => Tpm::None,
            },
            resilience,
        };
        let mut states = Vec::new();
        let mut cut = interrupt;
        while read_upgrade(&image).header.kind() == Kind::Upgrading {
            assert!(states.len() < 8, "{states:?}");
            let view = read_upgrade(&image);
            states.push(upgrade::check(&view).unwrap().0);
            let metadata = cs.metadata(&image).unwrap();
            let steps = upgrade::plan(&view, &gathered)
                .unwrap()
                .confirm(&metadata)
                .unwrap();
            for step in &steps {
                run_step(&cs, &image, step, cut);
            }
            cut &= !steps.iter().any(|s| matches!(s, Step::Reencrypt { .. }));
        }
        // The interrupted reencryption shows U2; the FIDO2 shape's U4 plan
        // ends the upgrade itself, the TPM shape's with the take, which
        // leaves U5.
        let mut expected = vec![State::U0, State::U1];
        if interrupt {
            expected.push(State::U2);
        }
        expected.extend([State::U3, State::U4]);
        if shape == Shape::Tpm {
            expected.push(State::U5);
        }
        assert_eq!(states, expected, "{case}");
        let view = read_upgrade(&image);
        assert_eq!(view.header.kind(), Kind::Protected);
        assert_eq!(protected_plan::check(&view.header), Ok(shape), "{case}");
        assert_ne!(view.volume.segment, Some(from));
        let theirs = luks2::metadata(&cs.metadata(&image).unwrap()).unwrap();
        assert_eq!(theirs, view.header.metadata());
        let mut opened = Vec::new();
        for (_, token) in &view.header.protected {
            let key = match token {
                Token::RecoveryKey { .. } => Key::Recovery,
                Token::Fido2(fido) => Key::Fido2 {
                    token: fido.credential[0],
                },
                Token::TpmPin(_) => Key::TpmPin,
                Token::Lockout(_) => continue,
                other => panic!("{other:?} remains"),
            };
            let slot = token.keyslot().unwrap();
            cs.run(&luks::test_args(&image, slot), &secret(key))
                .unwrap();
            let wrong = cs
                .run(&luks::test_args(&image, slot), &[0x33; 32])
                .unwrap_err();
            assert_eq!(luks::exit_code(&wrong), Some(luks::EXIT_BAD_PASSPHRASE));
            opened.push(slot);
        }
        opened.sort_unstable();
        assert_eq!(opened, view.header.keyslots, "{case}");
        let tpm_pin = usize::from(shape == Shape::Tpm);
        assert_eq!(opened.len(), 3 + tpm_pin, "{case}");
    }
}
