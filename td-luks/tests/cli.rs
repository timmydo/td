//! The td-luks program as td-protector's runner drives it (DESIGN.md
//! "Commands"): verbs on a regular file standing in for the device, key
//! material on standard input and through a `/proc/<pid>/fd/<n>` pipe,
//! exit statuses 0, 1 and 2, and cryptsetup's lock on the file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use td_fs::TempDir;
use td_json::Json;
use td_luks::header::metadata_json;
use td_luks::write::{self, DATA_OFFSET};

const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
const RECOVERY: &[u8] = b"000000000000000000000000000000000000000000000000";
const SECOND: &[u8] = b"ABCDEFGHABCDEFGHABCDEFGHABCDEFGH";
const WRONG: &[u8] = b"wrongwrongwrongwrongwrongwrongwr";
const TOKEN: &[u8] = br#"{"type":"td-protector","keyslots":["1"],"role":"first-boot"}"#;

struct Run {
    code: i32,
    stdout: Vec<u8>,
    stderr: String,
}

fn td_luks(args: &[&str], input: &[u8]) -> Run {
    td_luks_with(args, input, |command| {
        command.env_clear();
    })
}

fn td_luks_with(args: &[&str], input: &[u8], setup: impl FnOnce(&mut Command)) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_td-luks"));
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    setup(&mut command);
    let mut child = command.spawn().unwrap();
    // A refusal may exit before reading its input.
    let _ = child.stdin.take().unwrap().write_all(input);
    let output = child.wait_with_output().unwrap();
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// A key file only a descriptor holds, as td-protector's `KeyFile`.
struct KeyFile {
    _held: io::PipeReader,
    path: String,
}

fn key_file(secret: &[u8]) -> KeyFile {
    let (reader, mut writer) = io::pipe().unwrap();
    writer.write_all(secret).unwrap();
    drop(writer);
    let path = format!("/proc/{}/fd/{}", std::process::id(), reader.as_raw_fd());
    KeyFile {
        _held: reader,
        path,
    }
}

struct Disk {
    _dir: TempDir,
    root: PathBuf,
    device: PathBuf,
}

impl Disk {
    fn new(name: &str) -> Self {
        let dir = TempDir::new(&format!("td-luks-cli-{name}")).unwrap();
        let root = dir.path().to_path_buf();
        let device = root.join("device.img");
        File::create(&device)
            .unwrap()
            .set_len(DATA_OFFSET + (1 << 20))
            .unwrap();
        Self {
            _dir: dir,
            root,
            device,
        }
    }

    fn arg(&self) -> &str {
        self.device.to_str().unwrap()
    }

    fn formatted(name: &str) -> Self {
        let disk = Self::new(name);
        let run = td_luks(&["format", UUID, "td-system", disk.arg()], RECOVERY);
        assert_eq!(run.code, 0, "{}", run.stderr);
        disk
    }

    fn with_second_key(name: &str) -> Self {
        let disk = Self::formatted(name);
        let new = key_file(SECOND);
        let run = td_luks(&["add-key", "0", "1", disk.arg(), &new.path], RECOVERY);
        assert_eq!(run.code, 0, "{}", run.stderr);
        disk
    }

    fn metadata(&self) -> Json {
        let run = td_luks(&["metadata", self.arg()], b"");
        assert_eq!(run.code, 0, "{}", run.stderr);
        metadata_json(&run.stdout).unwrap()
    }

    fn header(&self) -> Vec<u8> {
        fs::read(&self.device).unwrap()[..DATA_OFFSET as usize].to_vec()
    }
}

fn library_volume_key(device: &Path, key: &[u8]) -> Vec<u8> {
    let mut file = File::open(device).unwrap();
    write::volume_key(&mut file, None, key)
        .unwrap()
        .bytes()
        .to_vec()
}

#[test]
fn format_add_and_test_keys_through_the_program() {
    let disk = Disk::with_second_key("keys");
    let json = disk.metadata();
    assert_eq!(
        json.get_path(&["config", "keyslots_size"])
            .and_then(Json::as_str),
        Some("16744448")
    );
    for (slot, key) in [("0", RECOVERY), ("1", SECOND)] {
        let run = td_luks(&["test", slot, disk.arg()], key);
        assert_eq!(run.code, 0, "{}", run.stderr);
        assert!(run.stderr.is_empty() && run.stdout.is_empty());
        assert_eq!(td_luks(&["test", slot, disk.arg()], WRONG).code, 2);
    }
    // Keyslot 1 authorizes keyslot 2, and a key that opens nothing
    // authorizes nothing.
    let third = key_file(b"third-third-third-third-third-th");
    assert_eq!(
        td_luks(&["add-key", "1", "2", disk.arg(), &third.path], SECOND).code,
        0
    );
    let fourth = key_file(SECOND);
    assert_eq!(
        td_luks(&["add-key", "1", "3", disk.arg(), &fourth.path], WRONG).code,
        2
    );
    let taken = key_file(SECOND);
    let run = td_luks(&["add-key", "0", "2", disk.arg(), &taken.path], RECOVERY);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("in use"), "{}", run.stderr);
}

#[test]
fn tokens_are_imported_replaced_and_removed() {
    let disk = Disk::with_second_key("tokens");
    assert_eq!(td_luks(&["token-import", "3", disk.arg()], TOKEN).code, 0);
    assert_eq!(td_luks(&["token-import", disk.arg()], TOKEN).code, 0);
    let json = disk.metadata();
    assert!(json.get_path(&["tokens", "3"]).is_some());
    // The lowest free number.
    assert!(json.get_path(&["tokens", "0"]).is_some());
    let run = td_luks(&["token-import", "3", disk.arg()], TOKEN);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("in use"), "{}", run.stderr);

    let replaced = br#"{"type":"td-protector","keyslots":["1"],"role":"device-bound"}"#;
    assert_eq!(
        td_luks(&["token-replace", "3", disk.arg()], replaced).code,
        0
    );
    assert_eq!(
        disk.metadata()
            .get_path(&["tokens", "3", "role"])
            .and_then(Json::as_str),
        Some("device-bound")
    );
    assert_eq!(td_luks(&["token-remove", "3", disk.arg()], b"").code, 0);
    assert!(disk.metadata().get_path(&["tokens", "3"]).is_none());
    assert_eq!(td_luks(&["token-remove", "3", disk.arg()], b"").code, 1);
    assert_eq!(td_luks(&["token-import", disk.arg()], b"[]").code, 1);
}

#[test]
fn kills_need_another_keyslot_s_key() {
    let disk = Disk::with_second_key("kill");
    assert_eq!(td_luks(&["token-import", "0", disk.arg()], TOKEN).code, 0);
    // Keyslot 1's own key does not kill it while keyslot 0 remains.
    assert_eq!(td_luks(&["kill-slot", "1", disk.arg()], SECOND).code, 2);
    let run = td_luks(&["kill-slot", "1", disk.arg()], RECOVERY);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let json = disk.metadata();
    assert!(json.get_path(&["keyslots", "1"]).is_none());
    assert_eq!(
        json.get_path(&["tokens", "0", "keyslots"]),
        Some(&Json::Arr(Vec::new()))
    );
    assert_eq!(td_luks(&["test", "1", disk.arg()], SECOND).code, 1);
    // Nothing unbound: a key that opens a bound keyslot removes nothing.
    assert_eq!(td_luks(&["kill-unbound", disk.arg()], RECOVERY).code, 0);
    assert_eq!(td_luks(&["kill-unbound", disk.arg()], WRONG).code, 2);
    assert!(disk.metadata().get_path(&["keyslots", "0"]).is_some());
}

#[test]
fn a_short_kill_key_is_refused_before_the_device_is_read() {
    let disk = Disk::with_second_key("short");
    let before = disk.header();
    for key in [&b""[..], &RECOVERY[..31]] {
        let run = td_luks(&["kill-slot", "1", disk.arg()], key);
        assert_eq!(run.code, 1);
        assert!(run.stderr.contains("at least 32 bytes"), "{}", run.stderr);
    }
    assert_eq!(disk.header(), before);
    // No device at all: the refusal still names the key.
    let missing = disk.root.join("missing.img");
    let run = td_luks(&["kill-slot", "1", missing.to_str().unwrap()], b"");
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("at least 32 bytes"), "{}", run.stderr);
}

#[test]
fn the_volume_key_file_is_new_private_and_the_key_s() {
    let disk = Disk::with_second_key("volume");
    let file = disk.root.join("volume.key");
    let file_arg = file.to_str().unwrap();
    let run = td_luks(&["volume-key", disk.arg(), file_arg], SECOND);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o400
    );
    let written = fs::read(&file).unwrap();
    assert_eq!(written.len(), 64);
    assert_eq!(written, library_volume_key(&disk.device, RECOVERY));
    // O_EXCL: an existing file is never overwritten.
    let run = td_luks(&["volume-key", disk.arg(), file_arg], RECOVERY);
    assert_eq!(run.code, 1);
    assert_eq!(fs::read(&file).unwrap(), written);
    // A wrong key creates no file.
    let none = disk.root.join("none.key");
    let run = td_luks(&["volume-key", disk.arg(), none.to_str().unwrap()], WRONG);
    assert_eq!(run.code, 2);
    assert!(!none.exists());
}

#[test]
fn the_detached_test_reads_the_header_file_and_writes_nothing() {
    let disk = Disk::with_second_key("detached");
    let header = disk.root.join("header.img");
    fs::write(&header, disk.header()).unwrap();
    let copy = fs::read(&header).unwrap();
    let args = ["test", "1", disk.arg(), header.to_str().unwrap()];
    assert_eq!(td_luks(&args, SECOND).code, 0);
    assert_eq!(td_luks(&args, WRONG).code, 2);
    assert_eq!(fs::read(&header).unwrap(), copy);
    // The detached form reads HEADER alone: a zeroed device does not
    // matter, a missing one does.
    File::create(&disk.device)
        .unwrap()
        .set_len(DATA_OFFSET)
        .unwrap();
    assert_eq!(td_luks(&args, SECOND).code, 0);
    fs::remove_file(&disk.device).unwrap();
    assert_eq!(td_luks(&args, SECOND).code, 1);
}

#[test]
fn the_grammar_refuses_everything_else() {
    let disk = Disk::formatted("grammar");
    let header = disk.header();
    for args in [
        &[][..],
        &["open", disk.arg(), "td-system"][..],
        &["status", "td-system"][..],
        &["close", "td-system"][..],
        &["metadata"][..],
        &["metadata", disk.arg(), "extra"][..],
        &["test", "32", disk.arg()][..],
        &["test", "01", disk.arg()][..],
        &["test", "-1", disk.arg()][..],
        &["add-key", "0", "1", disk.arg(), "/etc/passwd"][..],
        &["format", "NOT-A-UUID", "td-system", disk.arg()][..],
        &["format", UUID, "--cipher", "aes-cbc-essiv", disk.arg()][..],
    ] {
        let run = td_luks(args, RECOVERY);
        assert_eq!(run.code, 1, "{args:?}: {}", run.stderr);
        assert!(run.stderr.starts_with("td-luks: "), "{args:?}");
    }
    assert_eq!(disk.header(), header);
}

#[test]
fn input_past_one_pipe_page_is_refused() {
    let disk = Disk::formatted("bounded");
    let run = td_luks(&["test", "0", disk.arg()], &[b'0'; 4097]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("over 4096 bytes"), "{}", run.stderr);
    let long = key_file(&[b'k'; 4097]);
    let run = td_luks(&["add-key", "0", "1", disk.arg(), &long.path], RECOVERY);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("over 4096 bytes"), "{}", run.stderr);
}

#[test]
fn key_test_statuses_ignore_environment() {
    let disk = Disk::with_second_key("environment");
    for (code, key) in [(0, SECOND), (2, WRONG)] {
        let run = td_luks_with(&["test", "1", disk.arg()], key, |command| {
            command
                .env_clear()
                .env("CRYPTSETUP_LOCK_DIR", disk.root.to_str().unwrap())
                .env("TD_LUKS_KEY", "ABCDEFGHABCDEFGHABCDEFGHABCDEFGH")
                .env("LC_ALL", "de_DE.UTF-8");
        });
        assert_eq!(run.code, code, "{}", run.stderr);
    }
}

#[test]
fn no_message_carries_key_material() {
    let disk = Disk::with_second_key("quiet");
    let secret = "QUIETSECRETQUIETSECRETQUIETSECRE";
    let new = key_file(secret.as_bytes());
    for (args, input) in [
        (vec!["test", "0", disk.arg()], secret.as_bytes()),
        (vec!["kill-slot", "1", disk.arg()], secret.as_bytes()),
        (vec!["kill-slot", "1", disk.arg()], &secret.as_bytes()[..8]),
        (
            vec!["add-key", "1", "5", disk.arg(), &new.path],
            secret.as_bytes(),
        ),
    ] {
        let run = td_luks(&args, input);
        assert_ne!(run.code, 0, "{args:?}");
        assert!(
            !run.stderr.contains(&secret[..8]),
            "{args:?}: {}",
            run.stderr
        );
        assert!(run.stdout.is_empty());
    }
}

#[test]
fn a_held_lock_on_the_file_holds_off_the_program() {
    let disk = Disk::with_second_key("lock");
    // cryptsetup locks a regular file as itself: this exclusive flock is
    // the one a cryptsetup mutation of this file would hold.
    let holder = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&disk.device)
        .unwrap();
    holder.lock().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-luks"))
        .args(["metadata", disk.arg()])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "a read waits on a writer"
    );
    holder.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    // A shared holder lets a read through and holds off a mutation.
    holder.lock_shared().unwrap();
    assert_eq!(td_luks(&["metadata", disk.arg()], b"").code, 0);
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-luks"))
        .args(["token-import", disk.arg()])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(TOKEN).unwrap();
    thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "a mutation waits");
    holder.unlock().unwrap();
    assert!(child.wait().unwrap().success());
    assert!(disk.metadata().get_path(&["tokens", "0"]).is_some());

    // And the program's own lock holds off another holder while it runs:
    // its mutation's exclusive flock ends with it.
    assert!(holder.try_lock().is_ok());
}

#[test]
fn replacing_the_path_cannot_redirect_a_locked_mutation() {
    use std::os::unix::fs::MetadataExt;
    let disk = Disk::with_second_key("replace-locked");
    let replacement = disk.root.join("replacement.img");
    fs::copy(&disk.device, &replacement).unwrap();
    let held = File::open(&disk.device).unwrap();
    held.lock().unwrap();
    let other = File::open(&replacement).unwrap();
    other.lock().unwrap();
    let before = fs::read(&replacement).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-luks"))
        .args(["token-import", disk.arg()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(TOKEN).unwrap();
    // Observe its open descriptor before replacing the path, so this
    // exercises an actual waiter rather than racing process startup.
    let inode = held.metadata().unwrap().ino();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let observed = loop {
        let has_inode = fs::read_dir(format!("/proc/{}/fd", child.id()))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                fs::metadata(entry.path()).is_ok_and(|meta| {
                    meta.ino() == inode && meta.dev() == held.metadata().unwrap().dev()
                })
            });
        if has_inode {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let original = disk.root.join("original.img");
    fs::rename(&disk.device, &original).unwrap();
    fs::rename(&replacement, &disk.device).unwrap();
    held.unlock().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let finished = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(10));
    };
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(observed, "child opened the original inode");
    assert!(finished && output.status.success(), "{output:?}");
    assert_eq!(fs::read(&disk.device).unwrap(), before);
    let json = write::metadata(&mut File::open(original).unwrap()).unwrap();
    assert!(metadata_json(json.as_bytes())
        .unwrap()
        .get_path(&["tokens", "0"])
        .is_some());
}

#[test]
fn reencryption_refuses_regular_files_before_any_mutation() {
    let disk = Disk::formatted("reencrypt-file");
    let before = fs::read(&disk.device).unwrap();
    for args in [
        vec!["reencrypt", "0", "checksum", disk.arg(), "volume"],
        vec!["reencrypt", "0", "journal", disk.arg(), "volume"],
        vec!["reencrypt-resume", disk.arg(), "volume"],
    ] {
        let run = td_luks(&args, RECOVERY);
        assert_eq!(run.code, 1, "{}", run.stderr);
        assert!(
            run.stderr.contains("requires a block device"),
            "{}",
            run.stderr
        );
        assert_eq!(fs::read(&disk.device).unwrap(), before);
    }
}
