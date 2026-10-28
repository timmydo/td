//! Detached-header tests and key export through td-luks leave both the
//! source volume and the header copy unchanged, including a stale or
//! damaged secondary copy. Set TD_LUKS to the built program's absolute path.
//!
//! ```text
//! cargo test --frozen --manifest-path td-protector/Cargo.toml \
//!   --test keyslot_test -- --ignored
//! ```
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use td_protector::luks::{self, Bounded, KeyFile, Luks, EXIT_BAD_PASSPHRASE};
use td_protector::luks2;
use td_protector::protected::{Fido2, FidoRole, Token};

/// The recovery key's passphrase, keyslot 0's key.
const RECOVERY: &[u8] = b"000000000000000000000000000000000000000000000000";
/// The fido2 token's keyslot, and its key.
const FIDO_SLOT: u8 = 26;
const FIDO_KEY: [u8; 32] = [FIDO_SLOT; 32];
/// Where the second copy begins: td formats 16 KiB copies.
const SECONDARY: u64 = 0x4000;

fn scratch(name: &str) -> td_fs::TempDir {
    td_fs::TempDir::new(&format!("td-protector-keyslot-test-{name}")).unwrap()
}

#[path = "support/luks_program.rs"]
mod luks_program;

fn tool() -> Luks {
    Luks::new(luks_program::path())
}

fn import(cs: &Luks, image: &Path, id: u8, token: &Token) {
    let json = token.encode().unwrap();
    cs.run(&luks::token_import_args(image, Some(id)), json.as_bytes())
        .unwrap();
}

fn fido2(slot: u8) -> Token {
    Token::Fido2(Fido2 {
        keyslot: Some(slot),
        role: FidoRole::Recovery,
        credential: vec![0x5a; 64],
        salt: [0x5b; 32],
        x: [0x5c; 32],
        y: [0x5d; 32],
    })
}

fn bytes(path: &Path, from: u64, length: usize) -> Vec<u8> {
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(from)).unwrap();
    let mut out = vec![0; length];
    file.read_exact(&mut out).unwrap();
    out
}

fn overwrite(path: &Path, at: u64, with: &[u8]) {
    let mut file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(at)).unwrap();
    file.write_all(with).unwrap();
}

fn digest(path: &Path) -> [u8; 32] {
    td_tpm::digest(&fs::read(path).unwrap())
}

/// A td-formatted volume on a 20 MiB file, keyslot 0 the recovery key
/// with its marker and keyslot 26 a fido2 token's, each PBKDF2 as td
/// adds them, whose second copy is one sequence number behind, or
/// damaged.
fn volume(cs: &Luks, dir: &Path, damaged: bool) -> PathBuf {
    let image = dir.join("volume");
    File::create(&image).unwrap().set_len(20 << 20).unwrap();
    let args = luks::format_args("0f0e0d0c-0b0a-4908-8706-050403020100", "td-system", &image);
    cs.run(&args, RECOVERY).unwrap();
    let key = KeyFile::new(&FIDO_KEY).unwrap();
    cs.run(
        &luks::add_key_args(&image, 0, FIDO_SLOT, key.path()),
        RECOVERY,
    )
    .unwrap();
    import(cs, &image, 0, &Token::RecoveryKey { keyslot: Some(0) });
    let stale = bytes(&image, SECONDARY, SECONDARY as usize);
    import(cs, &image, 1, &fido2(FIDO_SLOT));
    if damaged {
        // A byte of the JSON area: the copy's checksum no longer holds.
        overwrite(&image, SECONDARY + 0x1000, b"!");
    } else {
        overwrite(&image, SECONDARY, &stale);
    }
    image
}

/// The test exactly as the storage worker runs it: the header area read
/// through `test_area`, copied, and named as the detached header with the volume
/// as the data device.
fn test(cs: &Luks, header: &Path, image: &Path, slot: u8, key: &[u8]) -> Bounded {
    let deadline = Instant::now() + Duration::from_secs(60);
    cs.run_bounded(
        &luks::detached_test_args(header, image, slot),
        key,
        deadline,
        &mut || false,
    )
    .unwrap()
}

fn untouched(damaged: bool) {
    let cs = tool();
    let dir = scratch(if damaged { "damaged" } else { "stale" });
    let image = volume(&cs, dir.path(), damaged);
    let area = luks2::test_area(&mut File::open(&image).unwrap()).unwrap();
    assert_eq!(area.header.keyslots, [0, FIDO_SLOT]);
    assert!(area.cost(0).is_ok() && area.cost(FIDO_SLOT).is_ok());
    let length = usize::try_from(area.length).unwrap();
    let before = digest(&image);
    let second = bytes(&image, SECONDARY, SECONDARY as usize);
    let cases: &[(u8, &[u8], Option<i32>)] = &[
        (0, RECOVERY, Some(0)),
        (0, b"000000000000000000000000000000000000000000000001", None),
        (FIDO_SLOT, &FIDO_KEY, Some(0)),
        (FIDO_SLOT, &[0; 32], None),
    ];
    for (slot, key, code) in cases {
        let header = dir.path().join("header");
        fs::write(&header, bytes(&image, 0, length)).unwrap();
        let ended = test(&cs, &header, &image, *slot, key);
        let Bounded::Exited(status) = ended else {
            panic!("keyslot {slot}: {ended:?}");
        };
        assert_eq!(
            status.code(),
            Some(code.unwrap_or(EXIT_BAD_PASSPHRASE)),
            "keyslot {slot}"
        );
        assert_eq!(digest(&image), before, "keyslot {slot} changed the volume");
        // Testing is read-only even when the selected copy is stale or damaged.
        assert_eq!(
            bytes(&header, SECONDARY, SECONDARY as usize),
            second,
            "keyslot {slot}: td-luks modified the detached copy"
        );
        fs::remove_file(&header).unwrap();
    }
}

#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn a_detached_test_leaves_a_volume_with_a_stale_second_copy_unchanged() {
    untouched(false);
}

#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn a_detached_test_leaves_a_volume_with_a_damaged_second_copy_unchanged() {
    untouched(true);
}

/// A request's dump exactly as the storage worker runs it: the tested
/// header copy as the device, a keyslot's key on standard input, into a
/// new file beside the copy. It reproduces the key the volume itself
/// dumps, through either keyslot, and refuses a name that exists.
#[test]
#[ignore = "set TD_LUKS to the built td-luks executable"]
fn a_dump_of_a_header_copy_gives_the_volumes_key() {
    let cs = tool();
    let dir = scratch("dump");
    let image = volume(&cs, dir.path(), false);
    let area = luks2::test_area(&mut File::open(&image).unwrap()).unwrap();
    area.all_costed().unwrap();
    let length = usize::try_from(area.length).unwrap();
    let copy = bytes(&image, 0, length);
    let dump = |device: &Path, file: &Path, key: &[u8]| {
        let deadline = Instant::now() + Duration::from_secs(60);
        cs.run_bounded(
            &luks::dump_volume_key_args(device, file),
            key,
            deadline,
            &mut || false,
        )
        .unwrap()
    };
    let succeeded = |ended: Bounded| matches!(ended, Bounded::Exited(status) if status.success());
    // Export also preserves the stale second copy on the image.
    let whole = dir.path().join("whole");
    assert!(succeeded(dump(&image, &whole, RECOVERY)));
    let expected = fs::read(&whole).unwrap();
    assert_eq!(expected.len(), 64);
    let header = dir.path().join("header");
    let file = dir.path().join("volume-key");
    for (slot, key) in [(0, RECOVERY), (FIDO_SLOT, &FIDO_KEY[..])] {
        // The copy as the worker leaves it: tested without repair.
        fs::write(&header, &copy).unwrap();
        assert!(succeeded(test(&cs, &header, &image, slot, key)));
        assert!(succeeded(dump(&header, &file, key)));
        assert_eq!(fs::read(&file).unwrap(), expected);
        fs::remove_file(&file).unwrap();
    }
    // A wrong key writes nothing; an existing name is left as it was.
    assert!(!succeeded(dump(&header, &file, &[0; 32])));
    assert!(!file.exists());
    fs::write(&file, b"x").unwrap();
    assert!(!succeeded(dump(&header, &file, RECOVERY)));
    assert_eq!(fs::read(&file).unwrap(), b"x");
}
