//! A keyslot test against a detached copy of the header leaves the disk
//! byte-for-byte unchanged, run by the pinned cryptsetup itself
//! (td-install/DESIGN.md, "Storage operation worker"): cryptsetup repairs
//! a stale or damaged second copy before it tries a key, and that repair
//! must land in the copy. Ignored by default, as `header_fit` is: it
//! needs the source-built cryptsetup and the host kernel's AF_ALG hash
//! and skcipher interfaces.
//!
//! ```text
//! TD_TEST_CRYPTSETUP=/absolute/path/to/cryptsetup cargo test --frozen \
//!   --manifest-path td-protector/Cargo.toml --test keyslot_test -- --ignored
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
use td_protector::cryptsetup::{self, Bounded, Cryptsetup, KeyFile, EXIT_BAD_PASSPHRASE};
use td_protector::luks2;
use td_protector::protected::{Fido2, FidoRole, Token};

/// The recovery key's passphrase, keyslot 0's key.
const RECOVERY: &[u8] = b"000000000000000000000000000000000000000000000000";
/// The fido2 token's keyslot, and its key.
const FIDO_SLOT: u8 = 26;
const FIDO_KEY: [u8; 32] = [FIDO_SLOT; 32];
/// Where the second copy begins: td formats 16 KiB copies.
const SECONDARY: u64 = 0x4000;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "td-protector-keyslot-test-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    Scratch(dir)
}

fn tool() -> Cryptsetup {
    let Some(path) = std::env::var_os("TD_TEST_CRYPTSETUP").map(PathBuf::from) else {
        panic!("TD_TEST_CRYPTSETUP names no cryptsetup");
    };
    assert!(path.is_absolute(), "TD_TEST_CRYPTSETUP must be absolute");
    Cryptsetup::new(path)
}

fn import(cs: &Cryptsetup, image: &Path, id: u8, token: &Token) {
    let json = token.encode().unwrap();
    cs.run(
        &cryptsetup::token_import_args(image, Some(id)),
        json.as_bytes(),
    )
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
fn volume(cs: &Cryptsetup, dir: &Path, damaged: bool) -> PathBuf {
    let image = dir.join("volume");
    File::create(&image).unwrap().set_len(20 << 20).unwrap();
    let args = cryptsetup::format_args("0f0e0d0c-0b0a-4908-8706-050403020100", "td-system", &image);
    cs.run(&args, RECOVERY).unwrap();
    let key = KeyFile::new(&FIDO_KEY).unwrap();
    cs.run(
        &cryptsetup::add_key_args(&image, 0, FIDO_SLOT, key.path()),
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
/// through `test_area`, copied, and named as `--header` with the volume
/// as the data device.
fn test(cs: &Cryptsetup, header: &Path, image: &Path, slot: u8, key: &[u8]) -> Bounded {
    let deadline = Instant::now() + Duration::from_secs(60);
    cs.run_bounded(
        &cryptsetup::detached_test_args(header, image, slot),
        key,
        deadline,
        &mut || false,
    )
    .unwrap()
}

fn untouched(damaged: bool) {
    let cs = tool();
    let dir = scratch(if damaged { "damaged" } else { "stale" });
    let image = volume(&cs, &dir.0, damaged);
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
        let header = dir.0.join("header");
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
        // The repair happened, in the copy.
        assert_ne!(
            bytes(&header, SECONDARY, SECONDARY as usize),
            second,
            "keyslot {slot}: cryptsetup repaired nothing"
        );
        fs::remove_file(&header).unwrap();
    }
}

#[test]
#[ignore = "needs TD_TEST_CRYPTSETUP and the kernel's AF_ALG"]
fn a_detached_test_leaves_a_volume_with_a_stale_second_copy_unchanged() {
    untouched(false);
}

#[test]
#[ignore = "needs TD_TEST_CRYPTSETUP and the kernel's AF_ALG"]
fn a_detached_test_leaves_a_volume_with_a_damaged_second_copy_unchanged() {
    untouched(true);
}
