//! The installed selector's release on a td LUKS2 volume
//! (td-install/ENCRYPTION.md "Selector release" and "Device-bound default"):
//! the TPM wait, td-protector's release order, the mapping it opens with the
//! released secret, and the volume key the handoff carries to td-kexec
//! ("Boot and authority boundaries"). Nothing here mounts or selects: the
//! caller does both only after `unlock` returns `Boot`.
use std::fs::{self, DirBuilder};
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::time::Duration;

use td_protector::cryptsetup::{dump_volume_key_args, open_args};
use td_protector::luks2::Header;
use td_protector::release::{self, Outcome, Runner, Tpm};
use td_protector::Secret;

use crate::protocol::VOLUME_KEY_BYTES;
use crate::td_fs::open_real_file;
use crate::unlock::{VolumeKey, TPM_DEVICE};

/// How long an installed selector booting an encrypted volume waits for the
/// TPM resource manager to appear: a late driver probe after it sees PCR 12
/// still open, which ENCRYPTION.md discloses.
pub(crate) const TPM_WAIT: Duration = Duration::from_secs(10);
const TPM_POLL: Duration = Duration::from_millis(100);
/// The device-mapper name the selector opens the volume under. The mapping
/// does not survive kexec; the deployment initramfs opens its own.
pub(crate) const MAPPING_NAME: &str = "td-selector";
/// A fresh mode-0700 directory in the selector's RAM-backed root, where
/// cryptsetup writes the volume key for the handoff; the key is read and
/// unlinked at once and the directory removed.
pub(crate) const KEY_DIRECTORY: &str = "/run/td-boot-volume-key";
const KEY_FILE: &str = "volume-key";

/// What the selector does after the release order.
pub(crate) enum Unlock {
    /// The mapping is open; boot with this key for the handoff.
    Boot { keyslot: u8, key: VolumeKey },
    /// Refuse boot and halt, naming the reason. Nothing secret is held.
    Halt(String),
    /// A cryptsetup step after a release failed; the boot fails as any
    /// other boot error does.
    Failed(io::Error),
}

/// Whether the TPM is present, waiting up to `TPM_WAIT` for it. Every
/// encrypted boot waits, whatever its header holds, so that corrupting the
/// header or stripping its td tokens cannot skip the wait and leave PCR 12
/// open to a late-probing TPM. `present` and `sleep` are the device probe
/// and the clock.
pub(crate) fn wait_for_tpm(
    present: &mut dyn FnMut() -> bool,
    sleep: &mut dyn FnMut(Duration),
) -> bool {
    if present() {
        return true;
    }
    let mut waited = Duration::ZERO;
    while waited < TPM_WAIT {
        sleep(TPM_POLL);
        waited = waited.saturating_add(TPM_POLL);
        if present() {
            return true;
        }
    }
    false
}

/// `/dev/tpmrm0` exists, as any node: whether it opens is the release's
/// question, and a device that will not open caps as uncertain and halts.
pub(crate) fn tpm_present() -> bool {
    fs::symlink_metadata(TPM_DEVICE).is_ok()
}

/// The release order over `header`, then what its outcome calls for.
/// `partition` names the pinned partition to cryptsetup, `/proc/PID/fd/N`;
/// `tpm` is `None` when no TPM device appeared within the wait.
pub(crate) fn unlock<T: Tpm, C: Runner>(
    header: &Result<Header, String>,
    partition: &Path,
    tpm: Option<&mut T>,
    cryptsetup: &mut C,
    key_directory: &Path,
    console: &mut dyn FnMut(&str),
) -> Unlock {
    let outcome = release::release(header, partition, tpm, cryptsetup, console);
    open(outcome, partition, cryptsetup, key_directory)
}

/// A released secret opens the mapping and then yields the volume key,
/// and is dropped, so zeroed, either way. Recovery is the next increment's
/// flow, so it refuses boot here, as a failed cap does.
pub(crate) fn open<C: Runner>(
    outcome: Outcome,
    partition: &Path,
    cryptsetup: &mut C,
    key_directory: &Path,
) -> Unlock {
    match outcome {
        Outcome::Released { keyslot, secret } => {
            if let Err(error) = cryptsetup.run(&open_args(partition, MAPPING_NAME), secret.expose())
            {
                return Unlock::Failed(error);
            }
            match volume_key(partition, &secret, cryptsetup, key_directory) {
                Ok(key) => Unlock::Boot { keyslot, key },
                Err(error) => Unlock::Failed(error),
            }
        }
        Outcome::Recovery { reason, .. } => Unlock::Halt(format!(
            "{reason}; the recovery flow is not yet implemented, so boot is refused"
        )),
        Outcome::Halt { reason } => Unlock::Halt(format!(
            "{reason}; every released secret is zeroed and boot is refused"
        )),
    }
}

/// cryptsetup writes the volume key into a fresh mode-0700 directory, the
/// key is read and the file unlinked at once, and the directory removed,
/// whether or not each step before succeeded.
fn volume_key<C: Runner>(
    partition: &Path,
    secret: &Secret,
    cryptsetup: &mut C,
    directory: &Path,
) -> io::Result<VolumeKey> {
    DirBuilder::new()
        .mode(0o700)
        .create(directory)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("volume key directory {}: {error}", directory.display()),
            )
        })?;
    let file = directory.join(KEY_FILE);
    let key = cryptsetup
        .run(&dump_volume_key_args(partition, &file), secret.expose())
        .and_then(|()| read_key(&file));
    let unlinked = match fs::remove_file(&file) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    };
    let removed = fs::remove_dir(directory);
    let key = key?;
    unlinked.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("unlink the volume key file {}: {error}", file.display()),
        )
    })?;
    removed.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "remove the volume key directory {}: {error}",
                directory.display()
            ),
        )
    })?;
    Ok(key)
}

/// Exactly `VOLUME_KEY_BYTES` from a real regular file, then end of file.
fn read_key(path: &Path) -> io::Result<VolumeKey> {
    let (mut file, metadata) = open_real_file(path, "volume key file")?;
    let refused = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the volume key file does not hold exactly {VOLUME_KEY_BYTES} bytes"),
        )
    };
    if metadata.len() != VOLUME_KEY_BYTES as u64 {
        return Err(refused());
    }
    let mut key = VolumeKey::zeroed();
    file.read_exact(key.fill()).map_err(|_| refused())?;
    let mut extra = [0u8; 1];
    if file.read(&mut extra)? != 0 {
        td_tpm::zero(&mut extra);
        return Err(refused());
    }
    Ok(key)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::cap::tests::{capped, read_reply, EXTEND_REPLY};
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use td_protector::luks2::HeaderCopy;
    use td_protector::release::Recovery;
    use td_protector::token::{Role, Token};
    use td_protector::CapError;
    use td_tpm::{Client, SealedObject, Transport};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// A fresh scratch directory, removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "td-boot-unlock-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn keys(&self) -> PathBuf {
            self.0.join("key-directory")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// What a stand-in cryptsetup does with `luksDump --dump-volume-key`.
    #[derive(Clone, Copy, PartialEq)]
    enum Dump {
        /// Writes the key as cryptsetup 2.8.8 does: create-new, mode 0400.
        Key,
        /// Writes this many bytes instead.
        Bytes(usize),
        /// Writes the key, then exits non-zero.
        WriteThenFail,
        /// Exits non-zero having written nothing.
        Fail,
    }

    /// A scripted cryptsetup: records every command with its standard input
    /// and whether the key file's directory existed, and fails `open` on
    /// request.
    struct Script {
        commands: Vec<(Vec<String>, Vec<u8>)>,
        open_fails: bool,
        dump: Dump,
        key: [u8; VOLUME_KEY_BYTES],
        directory_mode: Option<u32>,
    }

    impl Script {
        fn new(dump: Dump) -> Self {
            let mut key = [0u8; VOLUME_KEY_BYTES];
            for (index, byte) in key.iter_mut().enumerate() {
                *byte = index as u8 ^ 0xa5;
            }
            Self {
                commands: Vec::new(),
                open_fails: false,
                dump,
                key,
                directory_mode: None,
            }
        }
        fn verbs(&self) -> Vec<&str> {
            self.commands
                .iter()
                .map(|(args, _)| args.first().map(String::as_str).unwrap_or(""))
                .collect()
        }
    }

    impl Runner for Script {
        fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
            let words: Vec<String> = args
                .iter()
                .map(|arg| arg.to_str().unwrap().to_owned())
                .collect();
            self.commands.push((words.clone(), input.to_vec()));
            match words.first().map(String::as_str) {
                Some("open") if self.open_fails => Err(io::Error::other("open failed")),
                Some("luksDump") => {
                    let at = words.iter().position(|w| w == "--volume-key-file").unwrap();
                    let file = PathBuf::from(&words[at + 1]);
                    self.directory_mode =
                        Some(fs::metadata(file.parent().unwrap()).unwrap().mode() & 0o7777);
                    if self.dump == Dump::Fail {
                        return Err(io::Error::other("luksDump failed"));
                    }
                    let mut out = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o400)
                        .open(&file)?;
                    match self.dump {
                        Dump::Bytes(count) => out.write_all(&vec![7; count])?,
                        _ => out.write_all(&self.key)?,
                    }
                    if self.dump == Dump::WriteThenFail {
                        return Err(io::Error::other("luksDump failed"));
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        fn metadata(&mut self, _: &Path) -> io::Result<Vec<u8>> {
            Err(io::Error::other("no metadata in this script"))
        }
    }

    const PARTITION: &str = "/proc/1/fd/5";

    fn released() -> (Outcome, Vec<u8>) {
        let secret = Secret::generate().unwrap();
        let bytes = secret.expose().to_vec();
        (Outcome::Released { keyslot: 2, secret }, bytes)
    }

    /// A released secret opens the mapping by the held partition's name,
    /// then dumps the key into a fresh mode-0700 directory, both with the
    /// secret on standard input and never in argv; the key is read and
    /// both file and directory are gone.
    #[test]
    fn a_release_opens_the_mapping_then_takes_the_volume_key() {
        let scratch = Scratch::new();
        let mut script = Script::new(Dump::Key);
        let (outcome, secret) = released();
        let Unlock::Boot { keyslot, key } =
            open(outcome, Path::new(PARTITION), &mut script, &scratch.keys())
        else {
            panic!("the release did not boot");
        };
        assert_eq!(keyslot, 2);
        assert_eq!(key.expose(), script.key);
        assert_eq!(script.verbs(), ["open", "luksDump"]);
        assert_eq!(
            script.commands[0].0,
            [
                "open",
                "--type",
                "luks2",
                "--key-file=-",
                PARTITION,
                MAPPING_NAME
            ]
        );
        let file = scratch.keys().join(KEY_FILE);
        assert_eq!(
            script.commands[1].0,
            [
                "luksDump",
                "--dump-volume-key",
                "--batch-mode",
                "--volume-key-file",
                file.to_str().unwrap(),
                "--key-file=-",
                PARTITION,
            ]
        );
        for (args, input) in &script.commands {
            assert_eq!(input, &secret);
            for arg in args {
                assert!(!arg
                    .as_bytes()
                    .windows(8)
                    .any(|w| secret.windows(8).any(|s| s == w)));
            }
        }
        assert_eq!(script.directory_mode, Some(0o700));
        assert!(!file.exists() && !scratch.keys().exists());
    }

    /// The key directory must be fresh: one that exists is refused before
    /// cryptsetup is asked for the key, and is left as it was.
    #[test]
    fn an_existing_key_directory_is_refused() {
        let scratch = Scratch::new();
        fs::create_dir(scratch.keys()).unwrap();
        let mut script = Script::new(Dump::Key);
        let (outcome, _) = released();
        let Unlock::Failed(error) =
            open(outcome, Path::new(PARTITION), &mut script, &scratch.keys())
        else {
            panic!("an existing directory was used");
        };
        assert!(
            error.to_string().contains("volume key directory"),
            "{error}"
        );
        assert_eq!(script.verbs(), ["open"]);
        assert!(scratch.keys().exists());
    }

    /// A failed open dumps nothing; a dump that fails, writes the wrong
    /// length or writes and then fails leaves neither the file nor the
    /// directory behind.
    #[test]
    fn a_failed_open_or_dump_fails_the_boot_and_leaves_no_key() {
        let scratch = Scratch::new();
        let mut script = Script::new(Dump::Key);
        script.open_fails = true;
        let (outcome, _) = released();
        assert!(matches!(
            open(outcome, Path::new(PARTITION), &mut script, &scratch.keys()),
            Unlock::Failed(_)
        ));
        assert_eq!(script.verbs(), ["open"]);
        assert!(!scratch.keys().exists());
        for dump in [
            Dump::Fail,
            Dump::WriteThenFail,
            Dump::Bytes(63),
            Dump::Bytes(65),
            Dump::Bytes(0),
        ] {
            let mut script = Script::new(dump);
            let (outcome, _) = released();
            let Unlock::Failed(error) =
                open(outcome, Path::new(PARTITION), &mut script, &scratch.keys())
            else {
                panic!("a bad dump booted");
            };
            assert_eq!(script.verbs(), ["open", "luksDump"], "{error}");
            assert!(!scratch.keys().exists(), "{error}");
        }
    }

    /// Recovery and a failed cap refuse boot naming the reason, and run no
    /// cryptsetup command.
    #[test]
    fn recovery_and_a_failed_cap_halt_without_cryptsetup() {
        let scratch = Scratch::new();
        for (outcome, expected) in [
            (
                Outcome::Recovery {
                    reason: Recovery::NothingReleased,
                    reseal_offerable: true,
                },
                "no td protector released; the recovery flow is not yet implemented",
            ),
            (
                Outcome::Recovery {
                    reason: Recovery::NoTpm,
                    reseal_offerable: false,
                },
                "no TPM device",
            ),
            (
                Outcome::Halt {
                    reason: CapError::Mismatch,
                },
                "PCR 12 release cap readback mismatch",
            ),
        ] {
            let mut script = Script::new(Dump::Key);
            let Unlock::Halt(reason) =
                open(outcome, Path::new(PARTITION), &mut script, &scratch.keys())
            else {
                panic!("did not halt");
            };
            assert!(reason.starts_with(expected), "{reason}");
            assert!(script.commands.is_empty());
        }
    }

    /// The key file is read and unlinked, and its directory removed, before
    /// any of those steps' errors returns, so no path leaves it behind; and
    /// the released secret reaches cryptsetup only as standard input.
    #[test]
    fn the_key_file_is_read_and_unlinked_at_once() {
        let source = include_str!("selector_release.rs");
        let body = source
            .split_once("\nfn volume_key<")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        let position = |needle: &str| {
            assert_eq!(body.matches(needle).count(), 1, "{needle}");
            body.find(needle).unwrap()
        };
        let fresh = position(".mode(0o700)\n        .create(directory)");
        let dumped = position(".run(&dump_volume_key_args(partition, &file), secret.expose())");
        let read = position(".and_then(|()| read_key(&file));");
        let unlinked = position("let unlinked = match fs::remove_file(&file) {");
        let removed = position("let removed = fs::remove_dir(directory);");
        let first_return = position("let key = key?;");
        assert!(fresh < dumped && dumped < read && read < unlinked);
        assert!(unlinked < removed && removed < first_return);
        assert_eq!(body[dumped..first_return].matches('?').count(), 0);
        let open = source
            .split_once("\npub(crate) fn open<")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        assert!(open.contains(".run(&open_args(partition, MAPPING_NAME), secret.expose())"));
        assert!(open.find("open_args").unwrap() < open.find("volume_key(").unwrap());
        assert_eq!(KEY_DIRECTORY, "/run/td-boot-volume-key");
    }

    /// The wait polls for exactly `TPM_WAIT` and ends as soon as the device
    /// appears; a device already present is not waited for.
    #[test]
    fn the_tpm_wait_is_ten_seconds() {
        assert_eq!(TPM_WAIT, Duration::from_secs(10));
        assert_eq!(TPM_DEVICE, "/dev/tpmrm0");
        let mut slept = Duration::ZERO;
        let mut probes = 0;
        assert!(!wait_for_tpm(
            &mut || {
                probes += 1;
                false
            },
            &mut |step| slept += step
        ));
        assert_eq!(slept, TPM_WAIT);
        assert_eq!(probes, 101);
        let mut slept = Duration::ZERO;
        let mut probes = 0;
        assert!(wait_for_tpm(
            &mut || {
                probes += 1;
                probes == 4
            },
            &mut |step| slept += step
        ));
        assert_eq!(slept, Duration::from_millis(300));
        assert!(wait_for_tpm(&mut || true, &mut |_| panic!("waited")));
    }

    fn header(tokens: Vec<(u8, Token)>) -> Result<Header, String> {
        Ok(Header {
            copy: HeaderCopy::Primary,
            seqid: 1,
            hdr_size: 16384,
            uuid: "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a".into(),
            label: "td-system".into(),
            keyslots: vec![0, 1],
            tokens,
            orphans: Vec::new(),
            foreign: Vec::new(),
        })
    }

    fn first_boot_token() -> (u8, Token) {
        let sealed = SealedObject {
            public: vec![1; 8],
            private: vec![2; 8],
        };
        (0, Token::new(1, Role::FirstBoot, sealed).unwrap())
    }

    /// A TPM that answers PCR 12's read with `pcr12`, extends it, and
    /// refuses every other command, counting PCR_Extends and commands.
    struct Answering {
        pcr12: [u8; 32],
        extended: bool,
        commands: usize,
        extends: usize,
    }
    impl Answering {
        fn answer(&mut self, command: &[u8]) -> Vec<u8> {
            self.commands += 1;
            match command.get(6..10) {
                Some([0, 0, 1, 0x7e]) => {
                    read_reply(if self.extended { capped() } else { self.pcr12 })
                }
                Some([0, 0, 1, 0x82]) => {
                    self.extends += 1;
                    self.extended = true;
                    EXTEND_REPLY.to_vec()
                }
                // TPM_RC_FAILURE.
                _ => vec![0x80, 1, 0, 0, 0, 10, 0, 0, 1, 1],
            }
        }
    }

    /// A TPM whose every open fails: no token releases and the cap is
    /// uncertain.
    struct Unopenable(usize);
    impl Tpm for Unopenable {
        type Transport = td_tpm::Device;
        fn open(&mut self) -> Result<Client<td_tpm::Device>, String> {
            self.0 += 1;
            Err("open TPM resource manager: injected".into())
        }
    }

    /// A TPM each of whose clients answers from one shared state.
    struct Shared(std::rc::Rc<std::cell::RefCell<Answering>>);
    struct Handle(std::rc::Rc<std::cell::RefCell<Answering>>);
    impl Transport for Handle {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            Ok(self.0.borrow_mut().answer(command))
        }
    }
    impl Tpm for Shared {
        type Transport = Handle;
        fn open(&mut self) -> Result<Client<Handle>, String> {
            Ok(Client::new(Handle(self.0.clone())))
        }
    }

    fn shared(pcr12: [u8; 32]) -> (Shared, std::rc::Rc<std::cell::RefCell<Answering>>) {
        let state = std::rc::Rc::new(std::cell::RefCell::new(Answering {
            pcr12,
            extended: false,
            commands: 0,
            extends: 0,
        }));
        (Shared(state.clone()), state)
    }

    fn run_unlock<T: Tpm>(
        header: &Result<Header, String>,
        tpm: Option<&mut T>,
        script: &mut Script,
        scratch: &Scratch,
    ) -> (Unlock, Vec<String>) {
        let mut lines = Vec::new();
        let outcome = unlock(
            header,
            Path::new(PARTITION),
            tpm,
            script,
            &scratch.keys(),
            &mut |line| lines.push(line.to_owned()),
        );
        (outcome, lines)
    }

    /// Each outcome the release reaches without a released secret halts,
    /// with no cryptsetup command run and the cap extended at most once:
    /// no TPM (no cap), a first-boot token the TPM refuses (the cap closes,
    /// nothing released), PCR 12 already closed, and a TPM that will not
    /// open (an uncertain cap).
    #[test]
    fn every_unreleased_outcome_halts_before_cryptsetup() {
        let scratch = Scratch::new();
        let with_token = header(vec![first_boot_token()]);

        let mut script = Script::new(Dump::Key);
        let (outcome, lines) = run_unlock::<Shared>(&with_token, None, &mut script, &scratch);
        let Unlock::Halt(reason) = outcome else {
            panic!("no TPM booted")
        };
        assert!(reason.starts_with("no TPM device"), "{reason}");
        assert!(script.commands.is_empty(), "{lines:?}");

        let (mut tpm, state) = shared([0; 32]);
        let mut script = Script::new(Dump::Key);
        let (outcome, lines) = run_unlock(&with_token, Some(&mut tpm), &mut script, &scratch);
        let Unlock::Halt(reason) = outcome else {
            panic!("a refused token booted")
        };
        assert!(reason.starts_with("no td protector released"), "{reason}");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("td token 0 (first-boot)")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "PCR 12 release cap closed"),
            "{lines:?}"
        );
        assert_eq!(state.borrow().extends, 1);
        assert!(script.commands.is_empty());

        let (mut tpm, state) = shared([1; 32]);
        let mut script = Script::new(Dump::Key);
        let (outcome, _) = run_unlock(&header(Vec::new()), Some(&mut tpm), &mut script, &scratch);
        let Unlock::Halt(reason) = outcome else {
            panic!("a closed cap booted")
        };
        assert!(reason.starts_with("PCR 12 was already capped"), "{reason}");
        assert_eq!(state.borrow().extends, 0);
        assert!(script.commands.is_empty());

        let mut tpm = Unopenable(0);
        let mut script = Script::new(Dump::Key);
        let (outcome, _) = run_unlock(&with_token, Some(&mut tpm), &mut script, &scratch);
        let Unlock::Halt(reason) = outcome else {
            panic!("an uncertain cap booted")
        };
        assert!(reason.contains("platform reset required"), "{reason}");
        assert!(tpm.0 >= 2, "the token and the cap each opened the TPM");
        assert!(script.commands.is_empty());

        // A refused header is still capped, and halts.
        let (mut tpm, state) = shared([0; 32]);
        let mut script = Script::new(Dump::Key);
        let (outcome, _) = run_unlock(
            &Err("no valid copy".into()),
            Some(&mut tpm),
            &mut script,
            &scratch,
        );
        let Unlock::Halt(reason) = outcome else {
            panic!("a refused header booted")
        };
        assert!(
            reason.starts_with("the LUKS2 header is refused"),
            "{reason}"
        );
        assert_eq!(state.borrow().extends, 1);
        assert!(script.commands.is_empty());
        assert!(!scratch.keys().exists());
    }
}
