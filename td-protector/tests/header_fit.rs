//! The protected tier's largest LUKS2 headers, written by the pinned
//! cryptsetup on header files this test builds (td-install/ENCRYPTION.md,
//! "Re-encrypting upgrade" and increment 8a's acceptance evidence): each
//! must leave at least 1 KiB of the 12 KiB JSON area spare. Ignored by
//! default: it needs the source-built cryptsetup and the host kernel's
//! AF_ALG hash and skcipher interfaces, which the gate sandbox need not
//! provide.
//!
//! ```text
//! TD_TEST_CRYPTSETUP=/absolute/path/to/cryptsetup cargo test --frozen \
//!   --manifest-path td-protector/Cargo.toml --test header_fit -- --ignored
//! ```
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use td_protector::cryptsetup::{self, Cryptsetup, KeyFile};
use td_protector::luks2::{self, Kind, BINARY_HEADER_LEN, JSON_AREA_LEN};
use td_protector::pin::PinPcrs;
use td_protector::protected::{Fido2, FidoRole, Lockout, Operation, Shape, Token, TpmPin};
use td_tpm::SealedObject;

/// What every header must leave of the JSON area.
const SPARE: usize = 1024;
/// The recovery key's passphrase, keyslot 0's key.
const RECOVERY: &[u8] = b"000000000000000000000000000000000000000000000000";
/// The JSON text each header measured, cryptsetup 2.8.8 writing it. The
/// fixtures are fixed and every PBKDF parameter forced, so the sizes are
/// exact.
const PROTECTED_JSON: usize = 10360;
const UPGRADE_U2_JOURNAL_JSON: usize = 9696;
const UPGRADE_U2_CHECKSUM_JSON: usize = 9732;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "td-protector-header-fit-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    Scratch(dir)
}

fn tool() -> Option<Cryptsetup> {
    let path = PathBuf::from(std::env::var_os("TD_TEST_CRYPTSETUP")?);
    assert!(path.is_absolute(), "TD_TEST_CRYPTSETUP must be absolute");
    Some(Cryptsetup::new(path))
}

/// A td-formatted volume on a 20 MiB file: keyslot 0 the recovery key.
fn format(cs: &Cryptsetup, dir: &Path) -> PathBuf {
    let image = dir.join("volume");
    File::create(&image).unwrap().set_len(20 << 20).unwrap();
    let args = cryptsetup::format_args("0f0e0d0c-0b0a-4908-8706-050403020100", "td-system", &image);
    cs.run(&args, RECOVERY).unwrap();
    image
}

fn add_keyslot(cs: &Cryptsetup, image: &Path, slot: u8) {
    let key = KeyFile::new(&[slot; 32]).unwrap();
    cs.run(
        &cryptsetup::add_key_args(image, 0, slot, key.path()),
        RECOVERY,
    )
    .unwrap();
}

fn import(cs: &Cryptsetup, image: &Path, id: u8, json: &str) {
    cs.run(
        &cryptsetup::token_import_args(image, Some(id)),
        json.as_bytes(),
    )
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

fn request() -> Token {
    Token::Request {
        operation: Operation::PinChange,
    }
}

fn hex(len: usize) -> String {
    "ff".repeat(len)
}

/// The tokens increments 8e and 8i add, at the bounds their formats take
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
        format!(
            r#"{{"type":"td-protector","keyslots":[],"role":"update-retry","pair":"{}","tag":"{}"}}"#,
            hex(32),
            hex(32)
        ),
    ]
}

/// The largest protected state: keyslot 0, four fido2 keyslots and two
/// tpm-pin ones (a PIN change cut between its add and its retirement),
/// every token at its bound, the upgrade's own token beside them, then
/// 8e's and 8i's tokens. td's reader reads the header before the last
/// three, which it does not know yet.
#[test]
#[ignore = "needs TD_TEST_CRYPTSETUP and the kernel's AF_ALG"]
fn the_largest_protected_header_fits() {
    let Some(cs) = tool() else {
        panic!("TD_TEST_CRYPTSETUP names no cryptsetup");
    };
    let dir = scratch("protected");
    let image = format(&cs, &dir.0);
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
    let header = luks2::read(&mut File::open(&image).unwrap()).unwrap();
    assert_eq!(header.kind(), Kind::Upgrading);
    assert_eq!(header.protected.len(), 10);
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

fn reencrypt_init(cs: &Cryptsetup, image: &Path, resilience: &[&str]) {
    let mut args: Vec<OsString> = [
        "reencrypt",
        "--init-only",
        "--batch-mode",
        "--key-slot",
        "0",
        "--key-file=-",
        "--use-random",
        "--cipher",
        "aes-xts-plain64",
        "--key-size",
        "512",
        "--sector-size",
        "4096",
        "--hash",
        "sha256",
        "--pbkdf",
        "pbkdf2",
        "--pbkdf-force-iterations",
        "1000",
    ]
    .iter()
    .chain(resilience)
    .map(OsString::from)
    .collect();
    args.push(image.into());
    cs.run(&args, RECOVERY).unwrap();
}

/// U2 as the upgrade reaches it: keyslot 0, the four pending fido2 tokens
/// and the `td-upgrade` token, then cryptsetup's reencryption keyslot,
/// segments and second digest by `reencrypt --init-only`. td's reader
/// reads it as an upgrading header, as cryptsetup's metadata dump does.
#[test]
#[ignore = "needs TD_TEST_CRYPTSETUP and the kernel's AF_ALG"]
fn a_u2_header_reads_as_upgrading() {
    let Some(cs) = tool() else {
        panic!("TD_TEST_CRYPTSETUP names no cryptsetup");
    };
    let dir = scratch("u2-read");
    let image = format(&cs, &dir.0);
    for id in 0..4 {
        import(&cs, &image, id, &fido2(None).encode().unwrap());
    }
    import(&cs, &image, 4, &upgrade().encode().unwrap());
    reencrypt_init(&cs, &image, &["--resilience", "journal"]);
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
/// holds, then `reencrypt --init-only`, after which cryptsetup refuses a
/// token import.
fn upgrading_u2(name: &str, resilience: &[&str], expected: usize) {
    let Some(cs) = tool() else {
        panic!("TD_TEST_CRYPTSETUP names no cryptsetup");
    };
    let dir = scratch(name);
    let image = format(&cs, &dir.0);
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
#[ignore = "needs TD_TEST_CRYPTSETUP and the kernel's AF_ALG"]
fn the_largest_u2_header_fits_in_journal_resilience() {
    upgrading_u2(
        "u2-journal",
        &["--resilience", "journal"],
        UPGRADE_U2_JOURNAL_JSON,
    );
}

#[test]
#[ignore = "needs TD_TEST_CRYPTSETUP and the kernel's AF_ALG"]
fn the_largest_u2_header_fits_in_checksum_resilience() {
    upgrading_u2(
        "u2-checksum",
        &["--resilience", "checksum", "--resilience-hash", "sha256"],
        UPGRADE_U2_CHECKSUM_JSON,
    );
}
