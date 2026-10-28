//! td-luks and the pinned cryptsetup read each other's headers (DESIGN.md
//! "Tests"): what td-luks writes cryptsetup opens, dumps and leaves
//! unrepaired, and what cryptsetup writes with td's own arguments td-luks
//! opens to the same volume key. Ignored by default, as td-protector's
//! cryptsetup tests are: they need the pinned cryptsetup first on `PATH`
//! and the host kernel's AF_ALG hash and skcipher interfaces, and no
//! privilege, a regular file serving as the device.
//!
//! ```text
//! cargo test --manifest-path td-luks/Cargo.toml --test cryptsetup_interop -- --ignored
//! ```
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use td_fs::TempDir;
use td_json::Json;
use td_luks::header::metadata_json;
use td_luks::keyslot::{self, DevRandom, SPLIT_LEN};
use td_luks::write::{self, Error, TokenChange, DATA_OFFSET, KEYSLOTS_OFFSET};

#[path = "../../td-protector/tests/support/pinned_cryptsetup.rs"]
mod pinned_cryptsetup;

#[path = "../../td-encoding/src/lib.rs"]
#[allow(dead_code, reason = "the tests use hex and base64 alone")]
mod encoding;

const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
const RECOVERY: &[u8] = b"000000000000000000000000000000000000000000000000";
const SECOND: &[u8] = b"ABCDEFGHABCDEFGHABCDEFGHABCDEFGH";
const THIRD: &[u8] = b"zyxwvutsrqponmlkjihgfedcbaZYXWVU";
const WRONG: &[u8] = b"wrongwrongwrongwrongwrongwrongwr";
/// td-protector/src/cryptsetup.rs's `FORMAT` and `PBKDF`, word for word.
const FORMAT: &[&str] = &[
    "luksFormat",
    "--batch-mode",
    "--type",
    "luks2",
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
    "--use-random",
    "--luks2-metadata-size",
    "16384",
    "--luks2-keyslots-size",
    "16744448",
    "--offset",
    "32768",
    "--uuid",
    UUID,
    "--label",
    "td-system",
    "--key-slot",
    "0",
    "--key-file=-",
];
const PBKDF: &[&str] = &[
    "--pbkdf",
    "pbkdf2",
    "--pbkdf-force-iterations",
    "1000",
    "--hash",
    "sha256",
];

struct Fixture {
    dir: TempDir,
    cryptsetup: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let cryptsetup = pinned_cryptsetup::path();
        let dir = TempDir::new(&format!("td-luks-interop-{name}")).unwrap();
        Self { dir, cryptsetup }
    }

    /// A data device: the 16 MiB header area and 1 MiB of data.
    fn device(&self) -> PathBuf {
        let path = self.dir.path().join("device.img");
        let file = File::create(&path).unwrap();
        file.set_len(DATA_OFFSET + (1 << 20)).unwrap();
        path
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    /// cryptsetup's exit status and standard output.
    fn run(&self, args: &[&str], device: &Path, input: &[u8]) -> (i32, Vec<u8>) {
        self.run_tail(args, device, &[], input)
    }

    /// `run` with arguments after the device.
    fn run_tail(
        &self,
        args: &[&str],
        device: &Path,
        tail: &[&Path],
        input: &[u8],
    ) -> (i32, Vec<u8>) {
        let mut child = Command::new(&self.cryptsetup)
            .args(args)
            .arg(device)
            .args(tail)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        (output.status.code().unwrap_or(-1), output.stdout)
    }

    fn ok(&self, args: &[&str], device: &Path, input: &[u8]) -> Vec<u8> {
        let (code, out) = self.run(args, device, input);
        assert_eq!(code, 0, "cryptsetup {args:?}");
        out
    }

    fn dump(&self, device: &Path) -> Json {
        metadata_json(&self.ok(&["luksDump", "--dump-json-metadata"], device, b"")).unwrap()
    }

    fn test(&self, device: &Path, slot: u8, key: &[u8]) -> i32 {
        let slot = slot.to_string();
        let args = [
            "open",
            "--test-passphrase",
            "--type",
            "luks2",
            "--key-slot",
            &slot,
            "--key-file=-",
        ];
        self.run(&args, device, key).0
    }

    fn volume_key(&self, device: &Path, key: &[u8], name: &str) -> Vec<u8> {
        let out = self.dir.path().join(name);
        let out_arg = out.to_str().unwrap().to_owned();
        self.ok(
            &[
                "luksDump",
                "--dump-volume-key",
                "--batch-mode",
                "--volume-key-file",
                &out_arg,
                "--key-file=-",
            ],
            device,
            key,
        );
        fs::read(out).unwrap()
    }
}

fn open(device: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .unwrap()
}

fn header_bytes(device: &Path) -> Vec<u8> {
    fs::read(device).unwrap()[..DATA_OFFSET as usize].to_vec()
}

fn metadata(device: &Path) -> Json {
    td_json::parse_slice(write::metadata(&mut open(device)).unwrap().as_bytes()).unwrap()
}

#[test]
#[ignore = "needs the pinned cryptsetup on PATH and the host kernel's AF_ALG"]
fn cryptsetup_opens_what_td_luks_writes_and_repairs_nothing() {
    let fixture = Fixture::new("td-luks-writes");
    let device = fixture.device();
    let mut random = DevRandom;
    let mut file = open(&device);
    write::format(&mut file, &mut random, UUID, "td-system", RECOVERY).unwrap();
    write::add_key(&mut file, &mut random, 0, 1, RECOVERY, SECOND).unwrap();
    write::add_key(&mut file, &mut random, 1, 26, SECOND, THIRD).unwrap();
    let token = br#"{"type":"td-protector","keyslots":["1"],"role":"first-boot"}"#;
    write::token_change(&mut file, &mut random, TokenChange::Import(Some(0)), token).unwrap();
    drop(file);
    let written = header_bytes(&device);
    assert_eq!(fixture.dump(&device), metadata(&device));
    for (slot, key) in [(0, RECOVERY), (1, SECOND), (26, THIRD)] {
        assert_eq!(fixture.test(&device, slot, key), 0, "keyslot {slot}");
        assert_eq!(fixture.test(&device, slot, WRONG), 2, "keyslot {slot}");
    }
    let theirs = fixture.volume_key(&device, SECOND, "volume.key");
    let ours = write::volume_key(&mut open(&device), None, SECOND).unwrap();
    assert_eq!(theirs, ours.bytes());
    // cryptsetup rewrites a copy it finds stale or invalid; it found none.
    assert_eq!(header_bytes(&device), written);
}

#[test]
#[ignore = "needs the pinned cryptsetup on PATH and the host kernel's AF_ALG"]
fn td_luks_opens_what_cryptsetup_writes_with_td_s_arguments() {
    let fixture = Fixture::new("cryptsetup-writes");
    let device = fixture.device();
    fixture.ok(FORMAT, &device, RECOVERY);
    let second = fixture.file("second.key", SECOND);
    let second_arg = second.to_str().unwrap().to_owned();
    let mut add = vec!["luksAddKey", "--batch-mode"];
    add.extend(PBKDF);
    add.extend(["--key-slot", "0", "--new-key-slot", "1", "--key-file=-"]);
    let (code, _) = {
        let mut child = Command::new(&fixture.cryptsetup)
            .args(&add)
            .arg(&device)
            .arg(&second_arg)
            .env_clear()
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(RECOVERY).unwrap();
        let status = child.wait().unwrap();
        (status.code().unwrap_or(-1), ())
    };
    assert_eq!(code, 0);
    assert_eq!(metadata(&device), fixture.dump(&device));
    let mut file = open(&device);
    write::test(&mut file, 0, RECOVERY).unwrap();
    write::test(&mut file, 1, SECOND).unwrap();
    assert_eq!(write::test(&mut file, 1, WRONG), Err(Error::WrongKey));
    let ours = write::volume_key(&mut file, Some(1), SECOND).unwrap();
    assert_eq!(
        fixture.volume_key(&device, RECOVERY, "volume.key"),
        ours.bytes()
    );
}

#[test]
#[ignore = "needs the pinned cryptsetup on PATH and the host kernel's AF_ALG"]
fn td_luks_mutations_of_a_cryptsetup_header_keep_both_agreeing() {
    let fixture = Fixture::new("mutations");
    let device = fixture.device();
    fixture.ok(FORMAT, &device, RECOVERY);
    let mut random = DevRandom;
    let mut file = open(&device);
    write::add_key(&mut file, &mut random, 0, 1, RECOVERY, SECOND).unwrap();
    write::add_key(&mut file, &mut random, 0, 2, RECOVERY, THIRD).unwrap();
    let token = br#"{"type":"td-protector","keyslots":["1","2"],"role":"device-bound"}"#;
    write::token_change(&mut file, &mut random, TokenChange::Import(None), token).unwrap();
    // Killing keyslot 1 strips it from the token; the next add takes its
    // area again, as cryptsetup's first fit would.
    write::kill_slot(&mut file, &mut random, 1, RECOVERY).unwrap();
    write::add_key(&mut file, &mut random, 2, 5, THIRD, SECOND).unwrap();
    drop(file);
    let dump = fixture.dump(&device);
    assert_eq!(dump, metadata(&device));
    assert_eq!(
        dump.get_path(&["keyslots", "5", "area", "offset"])
            .and_then(Json::as_str),
        Some("290816")
    );
    assert_eq!(
        dump.get_path(&["tokens", "0", "keyslots"]),
        Some(&Json::Arr(vec![Json::from("2")]))
    );
    assert_eq!(fixture.test(&device, 1, SECOND), 1, "keyslot 1 is gone");
    for (slot, key) in [(0, RECOVERY), (2, THIRD), (5, SECOND)] {
        assert_eq!(fixture.test(&device, slot, key), 0, "keyslot {slot}");
    }
    // cryptsetup's own mutations of td-luks's header read back alike.
    fixture.ok(&["token", "remove", "--token-id", "0"], &device, b"");
    // luksKillSlot names its slot after the device.
    let five = {
        let mut child = Command::new(&fixture.cryptsetup)
            .args(["luksKillSlot", "--batch-mode", "--key-file=-"])
            .arg(&device)
            .arg("5")
            .env_clear()
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(RECOVERY).unwrap();
        child.wait().unwrap().code()
    };
    assert_eq!(five, Some(0));
    let ours = metadata(&device);
    assert_eq!(ours, fixture.dump(&device));
    assert!(ours.get_path(&["keyslots", "5"]).is_none());
    assert!(ours.get_path(&["tokens", "0"]).is_none());
    write::test(&mut open(&device), 2, THIRD).unwrap();
}

/// Captures the keyslot known answer pinned in src/keyslot.rs: a
/// cryptsetup `luksFormat` with td's arguments under RECOVERY, its
/// keyslot 0 KDF salt, the first 512-byte sector of its area, its
/// digest's salt and value, and the volume key cryptsetup dumps. The
/// sector's plaintext is td-luks's decryption, vouched for by the whole
/// area then merging to cryptsetup's volume key.
///
/// ```text
/// cargo test --manifest-path td-luks/Cargo.toml --test cryptsetup_interop \
///   capture_the_keyslot_known_answer -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs the pinned cryptsetup on PATH and the host kernel's AF_ALG"]
fn capture_the_keyslot_known_answer() {
    let fixture = Fixture::new("known-answer");
    let device = fixture.device();
    fixture.ok(FORMAT, &device, RECOVERY);
    let dump = fixture.dump(&device);
    let b64 = |path: &[&str]| {
        encoding::from_base64(dump.get_path(path).and_then(Json::as_str).unwrap()).unwrap()
    };
    let salt = b64(&["keyslots", "0", "kdf", "salt"]);
    let digest_salt = b64(&["digests", "0", "salt"]);
    let digest = b64(&["digests", "0", "digest"]);
    let volume_key = fixture.volume_key(&device, RECOVERY, "volume.key");
    let bytes = fs::read(&device).unwrap();
    let start = KEYSLOTS_OFFSET as usize;
    let area = &bytes[start..start + SPLIT_LEN];
    let mut split = area.to_vec();
    let area_key = keyslot::area_key(RECOVERY, &salt, keyslot::ITERATIONS).unwrap();
    keyslot::open(&area_key, &mut split).unwrap();
    assert_eq!(keyslot::merge(&split).unwrap().bytes()[..], volume_key[..]);
    for (name, value) in [
        ("SALT", &salt[..]),
        ("SECTOR", &area[..512]),
        ("PLAIN", &split[..512]),
        ("DIGEST_SALT", &digest_salt[..]),
        ("DIGEST", &digest[..]),
        ("VOLUME_KEY", &volume_key[..]),
    ] {
        println!("const {name}: &str = \"{}\";", encoding::hex(value));
    }
}

/// What cryptsetup reads back after td-luks leaves a token with no
/// keyslots, removes an unbound keyslot and kills the last keyslot; and
/// that td-luks treats cryptsetup's own unbound keyslot as cryptsetup
/// does.
#[test]
#[ignore = "needs the pinned cryptsetup on PATH and the host kernel's AF_ALG"]
fn orphaned_tokens_unbound_keyslots_and_no_keyslots_read_back() {
    let fixture = Fixture::new("orphans");
    let device = fixture.device();
    fixture.ok(FORMAT, &device, RECOVERY);
    // cryptsetup's unbound keyslot takes the lowest free number.
    let second = fixture.file("second.key", SECOND);
    let mut unbound = vec![
        "luksAddKey",
        "--batch-mode",
        "--unbound",
        "--key-size",
        "512",
    ];
    unbound.extend(PBKDF);
    assert_eq!(fixture.run_tail(&unbound, &device, &[&second], b"").0, 0);
    let dump = fixture.dump(&device);
    assert_eq!(
        dump.get_path(&["digests", "1", "segments"]),
        Some(&Json::Arr(vec![]))
    );
    let mut file = open(&device);
    assert_eq!(
        write::unbound(&write::load(&mut file).unwrap().json),
        Ok(vec![1])
    );
    // `test` agrees with `open --test-passphrase --key-slot 1`.
    write::test(&mut file, 1, SECOND).unwrap();
    assert_eq!(fixture.test(&device, 1, SECOND), 0);
    assert_eq!(write::test(&mut file, 1, WRONG), Err(Error::WrongKey));
    assert_eq!(fixture.test(&device, 1, WRONG), 2);
    // An add authorized by it fails in both, as no wrong key.
    let mut random = DevRandom;
    assert_eq!(
        write::add_key(&mut file, &mut random, 1, 2, SECOND, THIRD),
        Err(Error::Refused(
            "LUKS2 keyslot 1 is not bound to the volume's segment".into()
        ))
    );
    let third = fixture.file("third.key", THIRD);
    // With --new-key-slot, --key-slot names the authorizing keyslot.
    let mut add = vec![
        "luksAddKey",
        "--batch-mode",
        "--key-slot",
        "1",
        "--new-key-slot",
        "2",
        "--key-file=-",
    ];
    add.extend(PBKDF);
    assert_eq!(fixture.run_tail(&add, &device, &[&third], SECOND).0, 1);
    // A token left naming nothing.
    write::add_key(&mut file, &mut random, 0, 2, RECOVERY, THIRD).unwrap();
    let token = br#"{"type":"td-protector","keyslots":["2"],"role":"first-boot"}"#;
    write::token_change(&mut file, &mut random, TokenChange::Import(None), token).unwrap();
    write::kill_slot(&mut file, &mut random, 2, RECOVERY).unwrap();
    let ours = metadata(&device);
    assert_eq!(
        ours.get_path(&["tokens", "0", "keyslots"]),
        Some(&Json::Arr(vec![]))
    );
    assert_eq!(fixture.dump(&device), ours);
    // The unbound keyslot goes by the bound key; its digest stays empty.
    assert_eq!(write::kill_unbound(&mut file, &mut random, RECOVERY), Ok(1));
    let ours = metadata(&device);
    assert!(ours.get_path(&["keyslots", "1"]).is_none());
    assert_eq!(fixture.dump(&device), ours);
    assert_eq!(fixture.test(&device, 1, SECOND), 1);
    // The last keyslot, killed by its own key.
    write::kill_slot(&mut file, &mut random, 0, RECOVERY).unwrap();
    drop(file);
    let written = header_bytes(&device);
    let ours = metadata(&device);
    assert_eq!(ours.get_path(&["keyslots"]), Some(&Json::Obj(vec![])));
    assert_eq!(fixture.dump(&device), ours);
    let slot = fixture.test(&device, 0, RECOVERY);
    let any = fixture
        .run(
            &[
                "open",
                "--test-passphrase",
                "--type",
                "luks2",
                "--key-file=-",
            ],
            &device,
            RECOVERY,
        )
        .0;
    eprintln!("no keyslots: test-passphrase --key-slot 0 exits {slot}, with no slot {any}");
    assert_ne!(slot, 0);
    assert_ne!(any, 0);
    assert_eq!(header_bytes(&device), written);
}

/// The td-luks program's verbs, as td-protector's runner will run them.
fn program(args: &[&str], device: &Path, extra: &[&str], input: &[u8]) -> i32 {
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-luks"))
        .args(args)
        .arg(device)
        .args(extra)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(input);
    child.wait().unwrap().code().unwrap_or(-1)
}

/// A key file only a descriptor holds, as td-protector's `KeyFile`.
fn key_file(secret: &[u8]) -> (std::io::PipeReader, String) {
    use std::os::fd::AsRawFd;
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(secret).unwrap();
    drop(writer);
    let path = format!("/proc/{}/fd/{}", std::process::id(), reader.as_raw_fd());
    (reader, path)
}

#[test]
#[ignore = "needs the pinned cryptsetup on PATH and the host kernel's AF_ALG"]
fn the_program_and_cryptsetup_read_each_other_s_work() {
    let fixture = Fixture::new("program");
    // td-luks formats and adds; cryptsetup opens and dumps alike.
    let ours = fixture.device();
    assert_eq!(
        program(&["format", UUID, "td-system"], &ours, &[], RECOVERY),
        0
    );
    let (_held_second, second) = key_file(SECOND);
    assert_eq!(
        program(&["add-key", "0", "1"], &ours, &[&second], RECOVERY),
        0
    );
    let token = br#"{"type":"td-protector","keyslots":["1"],"role":"first-boot"}"#;
    assert_eq!(program(&["token-import", "0"], &ours, &[], token), 0);
    assert_eq!(fixture.dump(&ours), metadata(&ours));
    assert_eq!(fixture.test(&ours, 1, SECOND), 0);
    assert_eq!(fixture.test(&ours, 1, WRONG), 2);
    let key_path = fixture.dir.path().join("ours.key");
    assert_eq!(
        program(
            &["volume-key"],
            &ours,
            &[key_path.to_str().unwrap()],
            SECOND
        ),
        0
    );
    assert_eq!(
        fs::read(&key_path).unwrap(),
        fixture.volume_key(&ours, RECOVERY, "theirs-of-ours.key")
    );

    // cryptsetup formats with td's arguments; the program opens, adds,
    // kills and tests the detached header, and cryptsetup agrees.
    let theirs = fixture.dir.path().join("theirs.img");
    fs::rename(fixture.device(), &theirs).unwrap();
    fixture.ok(FORMAT, &theirs, RECOVERY);
    assert_eq!(program(&["test", "0"], &theirs, &[], RECOVERY), 0);
    assert_eq!(program(&["test", "0"], &theirs, &[], WRONG), 2);
    let (_held_third, third) = key_file(THIRD);
    assert_eq!(
        program(&["add-key", "0", "2"], &theirs, &[&third], RECOVERY),
        0
    );
    assert_eq!(fixture.test(&theirs, 2, THIRD), 0);
    assert_eq!(program(&["kill-slot", "2"], &theirs, &[], RECOVERY), 0);
    assert_eq!(fixture.test(&theirs, 2, THIRD), 1, "keyslot 2 is gone");
    assert_eq!(fixture.dump(&theirs), metadata(&theirs));
    let header = fixture.dir.path().join("theirs.header");
    fs::write(&header, header_bytes(&theirs)).unwrap();
    assert_eq!(
        program(
            &["test", "0"],
            &theirs,
            &[header.to_str().unwrap()],
            RECOVERY
        ),
        0
    );

    // cryptsetup locks a regular file as itself, as the program does:
    // while that flock is held exclusively, cryptsetup waits on it.
    let holder = open(&theirs);
    holder.lock().unwrap();
    let mut child = Command::new(&fixture.cryptsetup)
        .args(["luksDump", "--dump-json-metadata"])
        .arg(&theirs)
        .env_clear()
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "cryptsetup waits on the flock"
    );
    holder.unlock().unwrap();
    assert!(child.wait().unwrap().success());
}
