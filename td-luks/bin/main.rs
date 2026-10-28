//! The td-luks program (DESIGN.md "Commands"): one verb per operation
//! td-protector's runner performs, td's LUKS2 profile compiled in.
//!
//! The first secret is standard input, at most one pipe page; `add-key`'s
//! new key is a `/proc/<pid>/fd/<n>` descriptor the runner holds. Nothing
//! is read from the environment. The exit status is 0 on success, 2 for a
//! key that opens no candidate keyslot, 4 for an absent mapping, and 1
//! for anything else, the reason on
//! standard error and never key material.
#![deny(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-luks program requires Linux x86-64 open flags");

mod dm;
mod dm_sys;
mod lock;

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use td_luks::header::slot_number;
use td_luks::keyslot::DevRandom;
use td_luks::write::{self, Error, TokenChange, MAX_KEY, MIN_KILL_KEY};

use lock::Mode;

/// The volume key file's mode: cryptsetup's `--volume-key-file`.
const VOLUME_KEY_MODE: u32 = 0o400;
const USAGE: &str = "usage: td-luks format UUID LABEL DEVICE
       td-luks add-key BY NEW DEVICE NEW-KEY-FILE
       td-luks token-import [N] DEVICE
       td-luks token-replace N DEVICE
       td-luks token-remove N DEVICE
       td-luks kill-slot SLOT DEVICE
       td-luks kill-unbound DEVICE
       td-luks metadata DEVICE
       td-luks volume-key DEVICE FILE
       td-luks test SLOT DEVICE [HEADER]
       td-luks open [SLOT] DEVICE NAME
       td-luks open-volume-key DEVICE NAME KEY-FILE
       td-luks close NAME
       td-luks status NAME";

/// Why the program stopped, and its exit status.
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        let code = match error {
            Error::WrongKey => 2,
            Error::Refused(_) => 1,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

/// Key material read whole, zeroed when dropped.
struct Secret(Vec<u8>);

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Verb {
    Open {
        slot: Option<u8>,
        device: PathBuf,
        name: String,
    },
    OpenVolumeKey {
        device: PathBuf,
        name: String,
        file: PathBuf,
    },
    Close {
        name: String,
    },
    Status {
        name: String,
    },
    Format {
        uuid: String,
        label: String,
        device: PathBuf,
    },
    AddKey {
        by: u8,
        new: u8,
        device: PathBuf,
        new_key: PathBuf,
    },
    Token {
        change: TokenChange,
        device: PathBuf,
    },
    KillSlot {
        slot: u8,
        device: PathBuf,
    },
    KillUnbound {
        device: PathBuf,
    },
    Metadata {
        device: PathBuf,
    },
    VolumeKey {
        device: PathBuf,
        file: PathBuf,
    },
    Test {
        slot: u8,
        device: PathBuf,
        header: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let verb = args
        .first()
        .and_then(|verb| verb.to_str())
        .unwrap_or("")
        .to_owned();
    match parse(&args).and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            let _ = writeln!(io::stderr(), "td-luks: {verb}: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

fn usage() -> Failure {
    Failure::new(USAGE)
}

fn slot(arg: &OsStr) -> Result<u8, Failure> {
    arg.to_str()
        .and_then(slot_number)
        .ok_or_else(|| Failure::new(format!("{arg:?} is not a LUKS2 number 0 to 31")))
}

fn text(arg: &OsStr, what: &str) -> Result<String, Failure> {
    arg.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Failure::new(format!("the {what} is not UTF-8")))
}

/// A `/proc/<pid>/fd/<n>` or `/proc/self/fd/<n>` name: the only key
/// file the runner hands over.
fn descriptor(arg: &OsStr) -> Result<PathBuf, Failure> {
    let refuse = || Failure::new("the new key file must be a /proc/<pid>/fd/<n> descriptor");
    let name = arg.to_str().ok_or_else(refuse)?;
    let parts: Vec<&str> = name.split('/').collect();
    let digits = |part: &&str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    match parts.as_slice() {
        ["", "proc", pid, "fd", fd] if (*pid == "self" || digits(pid)) && digits(fd) => {
            Ok(PathBuf::from(name))
        }
        _ => Err(refuse()),
    }
}

fn parse(args: &[OsString]) -> Result<Verb, Failure> {
    let Some((verb, rest)) = args.split_first() else {
        return Err(usage());
    };
    let path = |arg: &OsString| PathBuf::from(arg);
    let verb = verb.to_str().ok_or_else(usage)?;
    let name = |arg: &OsString| -> Result<String, Failure> {
        let name = text(arg, "mapping name")?;
        if !dm_sys::valid_name(&name) {
            return Err(Failure::new("invalid mapping name"));
        }
        Ok(name)
    };
    Ok(match (verb, rest) {
        ("open", [device, mapping]) => Verb::Open {
            slot: None,
            device: path(device),
            name: name(mapping)?,
        },
        ("open", [number, device, mapping]) => Verb::Open {
            slot: Some(slot(number)?),
            device: path(device),
            name: name(mapping)?,
        },
        ("open-volume-key", [device, mapping, file]) => Verb::OpenVolumeKey {
            device: path(device),
            name: name(mapping)?,
            file: path(file),
        },
        ("close", [mapping]) => Verb::Close {
            name: name(mapping)?,
        },
        ("status", [mapping]) => Verb::Status {
            name: name(mapping)?,
        },
        ("format", [uuid, label, device]) => Verb::Format {
            uuid: text(uuid, "UUID")?,
            label: text(label, "label")?,
            device: path(device),
        },
        ("add-key", [by, new, device, new_key]) => Verb::AddKey {
            by: slot(by)?,
            new: slot(new)?,
            device: path(device),
            new_key: descriptor(new_key)?,
        },
        ("token-import", [device]) => Verb::Token {
            change: TokenChange::Import(None),
            device: path(device),
        },
        ("token-import", [id, device]) => Verb::Token {
            change: TokenChange::Import(Some(slot(id)?)),
            device: path(device),
        },
        ("token-replace", [id, device]) => Verb::Token {
            change: TokenChange::Replace(slot(id)?),
            device: path(device),
        },
        ("token-remove", [id, device]) => Verb::Token {
            change: TokenChange::Remove(slot(id)?),
            device: path(device),
        },
        ("kill-slot", [number, device]) => Verb::KillSlot {
            slot: slot(number)?,
            device: path(device),
        },
        ("kill-unbound", [device]) => Verb::KillUnbound {
            device: path(device),
        },
        ("metadata", [device]) => Verb::Metadata {
            device: path(device),
        },
        ("volume-key", [device, file]) => Verb::VolumeKey {
            device: path(device),
            file: path(file),
        },
        ("test", [number, device]) => Verb::Test {
            slot: slot(number)?,
            device: path(device),
            header: None,
        },
        ("test", [number, device, header]) => Verb::Test {
            slot: slot(number)?,
            device: path(device),
            header: Some(path(header)),
        },
        _ => return Err(usage()),
    })
}

/// All of `reader`, refused past one pipe page.
fn bounded(mut reader: impl Read, what: &str) -> Result<Secret, Failure> {
    // One allocation, with no reallocations or buffered copies of secrets.
    let mut bytes = Secret(vec![0; MAX_KEY + 1]);
    let mut used = 0;
    while used < bytes.0.len() {
        let tail = bytes
            .0
            .get_mut(used..)
            .ok_or_else(|| Failure::new("input bound"))?;
        match reader.read(tail) {
            Ok(0) => break,
            Ok(count) => used += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Failure::new(format!("reading {what}: {error}"))),
        }
    }
    if used > MAX_KEY {
        return Err(Failure::new(format!("{what} is over {MAX_KEY} bytes")));
    }
    bytes.0.truncate(used);
    Ok(bytes)
}

fn stdin() -> Result<Secret, Failure> {
    // Read the descriptor directly: Stdin's global BufReader keeps copies.
    let descriptor = io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|error| Failure::new(format!("opening standard input: {error}")))?;
    bounded(File::from(descriptor), "standard input")
}

fn locked(device: &Path, mode: Mode, exclusive: bool) -> Result<(File, lock::Lock), Failure> {
    lock::open(device, mode, exclusive)
        .map_err(|error| Failure::new(format!("locking {}: {error}", device.display())))
}

fn open(device: &Path) -> Result<File, Failure> {
    File::open(device)
        .map_err(|error| Failure::new(format!("opening {}: {error}", device.display())))
}

fn run(verb: Verb) -> Result<(), Failure> {
    let mut random = DevRandom;
    match verb {
        Verb::Open { slot, device, name } => {
            let key = stdin()?;
            activate(
                &device,
                &name,
                write::ActivationKey::Passphrase {
                    slot,
                    bytes: &key.0,
                },
            )?;
        }
        Verb::OpenVolumeKey { device, name, file } => {
            let key = bounded(open(&file)?, "volume key")?;
            activate(&device, &name, write::ActivationKey::Volume(&key.0))?;
        }
        Verb::Close { name } => dm::close(&name).map_err(dm_failure)?,
        Verb::Status { name } => {
            let status = dm::status(&name)
                .map_err(dm_failure)?
                .ok_or_else(|| Failure {
                    code: 4,
                    message: "mapping is absent".into(),
                })?;
            writeln!(
                io::stdout(),
                "{} {} {}",
                status.name,
                status.uuid,
                if status.suspended {
                    "suspended"
                } else if status.active {
                    "active"
                } else {
                    "inactive"
                }
            )
            .map_err(dm_failure)?;
        }
        Verb::Format {
            uuid,
            label,
            device,
        } => {
            let passphrase = stdin()?;
            let (mut medium, _lock) = locked(&device, Mode::Write, true)?;
            write::format(&mut medium, &mut random, &uuid, &label, &passphrase.0)?;
        }
        Verb::AddKey {
            by,
            new,
            device,
            new_key,
        } => {
            let key = stdin()?;
            let new_key = File::open(&new_key)
                .map_err(|error| Failure::new(format!("opening the new key file: {error}")))
                .and_then(|file| bounded(file, "the new key file"))?;
            let (mut medium, _lock) = locked(&device, Mode::Write, false)?;
            write::add_key(&mut medium, &mut random, by, new, &key.0, &new_key.0)?;
        }
        Verb::Token { change, device } => {
            let text = match change {
                TokenChange::Remove(_) => Secret(Vec::new()),
                _ => stdin()?,
            };
            let (mut medium, _lock) = locked(&device, Mode::Write, false)?;
            write::token_change(&mut medium, &mut random, change, &text.0)?;
        }
        Verb::KillSlot { slot, device } => {
            let key = stdin()?;
            // Before the device is touched: cryptsetup kills unasked on
            // empty input.
            if key.0.len() < MIN_KILL_KEY {
                return Err(Failure::new(format!(
                    "a kill takes a key of at least {MIN_KILL_KEY} bytes"
                )));
            }
            let (mut medium, _lock) = locked(&device, Mode::Write, false)?;
            write::kill_slot(&mut medium, &mut random, slot, &key.0)?;
        }
        Verb::KillUnbound { device } => {
            let key = stdin()?;
            let (mut medium, _lock) = locked(&device, Mode::Write, false)?;
            write::kill_unbound(&mut medium, &mut random, &key.0)?;
        }
        Verb::Metadata { device } => {
            let json = {
                let (mut medium, _lock) = locked(&device, Mode::Read, false)?;
                write::metadata(&mut medium)?
            };
            let mut out = io::stdout().lock();
            writeln!(out, "{json}")
                .and_then(|()| out.flush())
                .map_err(|error| Failure::new(format!("writing the metadata: {error}")))?;
        }
        Verb::VolumeKey { device, file } => {
            let key = stdin()?;
            let volume_key = {
                let (mut medium, _lock) = locked(&device, Mode::Read, false)?;
                write::volume_key(&mut medium, None, &key.0)?
            };
            write_volume_key(&file, volume_key.bytes())?;
        }
        Verb::Test {
            slot,
            device,
            header,
        } => {
            let key = stdin()?;
            match header {
                // The detached form reads HEADER alone and writes nothing.
                Some(header) => {
                    fs::metadata(&device)
                        .map_err(|error| Failure::new(format!("{}: {error}", device.display())))?;
                    write::test(&mut open(&header)?, slot, &key.0)?;
                }
                None => {
                    let (mut medium, _lock) = locked(&device, Mode::Read, false)?;
                    write::test(&mut medium, slot, &key.0)?;
                }
            }
        }
    }
    Ok(())
}

fn dm_failure(error: io::Error) -> Failure {
    Failure::new(format!("device mapper: {error}"))
}

fn activate(
    device: &Path,
    name: &str,
    credential: write::ActivationKey<'_>,
) -> Result<(), Failure> {
    let (mut file, _lock) = locked(device, Mode::Read, false)?;
    let (header, key) = write::activation(&mut file, credential)?;
    dm::activate(&mut file, name, &header.uuid, key.bytes()).map_err(dm_failure)
}

/// The volume key into a new file of mode 0400, which must not exist;
/// a failed write leaves no file.
fn write_volume_key(path: &Path, key: &[u8]) -> Result<(), Failure> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(VOLUME_KEY_MODE)
        .open(path)
        .map_err(|error| Failure::new(format!("creating {}: {error}", path.display())))?;
    if let Err(error) = file.write_all(key).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(Failure::new(format!("writing {}: {error}", path.display())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn short_reads_use_only_the_preallocated_secret_buffer() {
        struct Chunks {
            used: usize,
            base: Option<usize>,
        }
        impl Read for Chunks {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                assert_eq!(out.len(), MAX_KEY + 1 - self.used);
                let base = *self.base.get_or_insert(out.as_ptr() as usize);
                assert_eq!(out.as_ptr() as usize, base + self.used);
                if self.used == 48 {
                    return Ok(0);
                }
                out[0] = b'k';
                self.used += 1;
                Ok(1)
            }
        }
        let key = bounded(
            Chunks {
                used: 0,
                base: None,
            },
            "fixture",
        )
        .unwrap_or_else(|_| panic!("bounded read"));
        assert_eq!(key.0, vec![b'k'; 48]);
    }

    #[test]
    fn every_verb_parses_exactly_its_grammar() {
        assert_eq!(
            parse(&args(&["add-key", "0", "31", "/dev/sda2", "/proc/77/fd/4"]))
                .ok()
                .unwrap(),
            Verb::AddKey {
                by: 0,
                new: 31,
                device: "/dev/sda2".into(),
                new_key: "/proc/77/fd/4".into(),
            }
        );
        assert_eq!(
            parse(&args(&["token-import", "/dev/sda2"])).ok().unwrap(),
            Verb::Token {
                change: TokenChange::Import(None),
                device: "/dev/sda2".into(),
            }
        );
        assert_eq!(
            parse(&args(&["test", "1", "/dev/sda2", "/tmp/header"]))
                .ok()
                .unwrap(),
            Verb::Test {
                slot: 1,
                device: "/dev/sda2".into(),
                header: Some("/tmp/header".into()),
            }
        );
        for refused in [
            &[][..],
            &["reencrypt", "0", "checksum", "/dev/sda2", "name"][..],
            &["format", "u", "l"][..],
            &["metadata", "/dev/sda2", "extra"][..],
            &["kill-slot", "32", "/dev/sda2"][..],
            &["kill-slot", "01", "/dev/sda2"][..],
            &["kill-slot", "+1", "/dev/sda2"][..],
            &["token-replace", "/dev/sda2"][..],
            &["add-key", "0", "1", "/dev/sda2", "/etc/key"][..],
            &["add-key", "0", "1", "/dev/sda2", "/proc/1/fd/x"][..],
            &["add-key", "0", "1", "/dev/sda2", "/proc/1/fd/4/.."][..],
            &["--cipher", "aes-cbc-essiv"][..],
        ] {
            assert!(parse(&args(refused)).is_err(), "{refused:?}");
        }
        assert!(parse(&args(&["add-key", "0", "1", "d", "/proc/self/fd/9"])).is_ok());
    }
}
