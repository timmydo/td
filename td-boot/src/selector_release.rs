//! The installed selector's release on a td LUKS2 volume
//! (td-install/ENCRYPTION.md "Selector release" and "Device-bound default"):
//! the TPM wait, td-protector's release order, the recovery flow when it
//! ends in recovery, the mapping opened with the released secret or the
//! recovery key, and the volume key the handoff carries to td-kexec
//! ("Boot and authority boundaries"). Nothing here mounts or selects: the
//! caller does both only after `unlock` returns `Boot`.
use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use td_protector::cryptsetup::{
    dump_volume_key_args, exit_code, open_args, open_slot_args, test_args, EXIT_BAD_PASSPHRASE,
};
use td_protector::luks2::Header;
use td_protector::recovery::RecoveryKey;
use td_protector::release::{self, Outcome, Resealed, Runner, Tpm};
use td_protector::transition::RECOVERY_SLOT;

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

/// The recovery-key prompt. secret-line prints it, only once echo is off
/// and what was typed before it is discarded; td-boot never does.
pub(crate) const KEY_PROMPT: &str = "td recovery key: ";
/// The reseal question, asked through secret-line too, so that nothing
/// typed before it is shown can answer it.
pub(crate) const RESEAL_PROMPT: &str =
    "Type reseal to seal a protector to this boot chain, anything else to boot once: ";
/// What confirming the reseal does, printed before the question.
pub(crate) const RESEAL_WARNING: &str =
    "a reseal binds release to this boot chain and retires every other td protector; \
     keyslot 0's recovery key stays";
/// The line naming the entry, printed before the first recovery prompt. The
/// VT reads key positions under the kernel's built-in US keymap, whatever
/// the keyboard's legends, and starts with Num Lock off
/// (td-install/ENCRYPTION.md "Keyboard console").
pub(crate) const ENTRY_KEYS: &str =
    "recovery: enter the recovery key's 48 digits on the keyboard's top row, unshifted, \
     spaces or hyphens between groups optional, keypad digits only with Num Lock on; \
     it is not shown as it is typed";
/// Printed before the reseal question: on a keyboard whose letters sit
/// elsewhere, the answer is typed where a US keyboard has them.
pub(crate) const RESEAL_KEYS: &str =
    "type the answer at its US key positions: the keyboard is read as a US one";
/// The one answer that confirms the reseal: no default, and no other word.
pub(crate) const RESEAL_WORD: &[u8] = b"reseal";
/// secret-line's longest line (its `LINE_MAX`).
pub(crate) const ENTRY_MAX: usize = 256;
/// secret-line's status at end of input before a newline (`EXIT_EOF`).
const EXIT_EOF: i32 = 3;
/// secret-line's status for a record longer than `ENTRY_MAX` or holding a
/// newline before its end (`EXIT_REFUSED`).
const EXIT_REFUSED: i32 = 4;

/// What the selector does after the release order.
pub(crate) enum Unlock {
    /// The mapping is open; boot with this key for the handoff.
    Boot { keyslot: u8, key: VolumeKey },
    /// Refuse boot and halt, naming the reason. Nothing secret is held.
    Halt(String),
    /// A cryptsetup step after a release, or after a recovery key opened
    /// keyslot 0, failed; the boot fails as any other boot error does.
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

/// One line secret-line read, in one heap allocation zeroed on drop. It is
/// neither `Debug`, `Display` nor `Clone`.
pub(crate) struct Entry {
    bytes: Box<[u8; ENTRY_MAX]>,
    len: usize,
}

impl Drop for Entry {
    fn drop(&mut self) {
        td_tpm::zero(self.bytes.as_mut_slice());
        self.len = 0;
        #[cfg(test)]
        tests::LIVE_ENTRIES.with(|live| live.set(live.get() - 1));
    }
}

impl Entry {
    fn new() -> Self {
        #[cfg(test)]
        tests::LIVE_ENTRIES.with(|live| live.set(live.get() + 1));
        Self {
            bytes: Box::new([0; ENTRY_MAX]),
            len: 0,
        }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }

    /// Everything `reader` gives until end of file, at most `ENTRY_MAX`
    /// bytes; more is refused, never truncated.
    fn fill_from(&mut self, reader: &mut dyn Read) -> io::Result<()> {
        loop {
            let rest = self.bytes.get_mut(self.len..).unwrap_or(&mut []);
            if rest.is_empty() {
                let mut extra = [0u8; 1];
                let more = reader.read(&mut extra);
                td_tpm::zero(&mut extra);
                return match more? {
                    0 => Ok(()),
                    _ => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("secret-line wrote more than {ENTRY_MAX} bytes"),
                    )),
                };
            }
            match reader.read(rest) {
                Ok(0) => return Ok(()),
                Ok(count) => self.len = self.len.saturating_add(count).min(ENTRY_MAX),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// How one run of secret-line ended.
pub(crate) enum Entered {
    /// A whole line, without its newline.
    Line(Entry),
    /// End of input before a newline (`^D`, or a hung-up console).
    Eof,
    /// A record over `ENTRY_MAX` bytes or with a newline inside, refused
    /// whole.
    Refused,
}

/// The console reader: one run of td-init's secret-line per entry.
pub(crate) trait SecretLine {
    /// Run it with `prompt`, which it prints once echo is off.
    fn read(&mut self, prompt: &str) -> io::Result<Entered>;
}

/// The secret-line applet at its absolute path. It reads `/dev/console`
/// itself, with echo off; its standard output is a std pipe this process
/// reads, its environment is cleared and it inherits standard error.
pub(crate) struct Applet(pub(crate) PathBuf);

impl SecretLine for Applet {
    fn read(&mut self, prompt: &str) -> io::Result<Entered> {
        let program = self.0.display().to_string();
        let named = |error: io::Error| io::Error::new(error.kind(), format!("{program}: {error}"));
        let mut child = Command::new(&self.0)
            .arg(prompt)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(named)?;
        let mut entry = Entry::new();
        // The pipe's read end is dropped here, before the wait, so a child
        // that would write more finds no reader rather than block.
        let filled = match child.stdout.take() {
            Some(mut out) => entry.fill_from(&mut out),
            None => Err(io::Error::other("no pipe from the applet")),
        };
        let status = child.wait().map_err(named)?;
        filled.map_err(named)?;
        match status.code() {
            Some(0) => Ok(Entered::Line(entry)),
            Some(EXIT_EOF) => Ok(Entered::Eof),
            Some(EXIT_REFUSED) => Ok(Entered::Refused),
            _ => Err(io::Error::other(format!("{program}: {status}"))),
        }
    }
}

/// The release order over `header`, then what its outcome calls for: a
/// released secret opens the mapping, recovery runs the recovery flow on
/// `line`, and an uncertain cap halts. `partition` names the pinned
/// partition to cryptsetup, `/proc/PID/fd/N`; `tpm` is `None` when no TPM
/// device appeared within the wait.
pub(crate) fn unlock<T: Tpm, C: Runner>(
    header: &Result<Header, String>,
    partition: &Path,
    mut tpm: Option<&mut T>,
    cryptsetup: &mut C,
    line: &mut dyn SecretLine,
    key_directory: &Path,
    console: &mut dyn FnMut(&str),
) -> Unlock {
    match release::release(header, partition, tpm.as_deref_mut(), cryptsetup, console) {
        Outcome::Released { keyslot, secret } => open(
            partition,
            keyslot,
            &open_args(partition, MAPPING_NAME),
            secret.expose(),
            cryptsetup,
            key_directory,
        ),
        // The release has already printed the reason.
        Outcome::Recovery {
            reseal_offerable, ..
        } => {
            let offer = if reseal_offerable {
                header.as_ref().ok()
            } else {
                None
            };
            let mut reseal = |header: &Header,
                              key: &RecoveryKey,
                              cryptsetup: &mut C,
                              console: &mut dyn FnMut(&str)| {
                match tpm.as_deref_mut() {
                    Some(tpm) => release::reseal(header, partition, key, tpm, cryptsetup, console),
                    None => Resealed::NotRun("no TPM device".into()),
                }
            };
            recover(
                offer,
                partition,
                cryptsetup,
                line,
                &mut reseal,
                key_directory,
                console,
            )
        }
        Outcome::Halt { reason } => Unlock::Halt(format!(
            "{reason}; every released secret is zeroed and boot is refused"
        )),
    }
}

/// The reseal td-protector runs: the header it plans over, the recovery
/// key that opened keyslot 0, the runner and the console.
type Reseal<'a, C> =
    dyn FnMut(&Header, &RecoveryKey, &mut C, &mut dyn FnMut(&str)) -> Resealed + 'a;

/// The recovery flow on the selector's console (ENCRYPTION.md
/// "Device-bound default"). Each entry is one run of secret-line, which
/// prints the prompt. An entry the recovery-key codec refuses prompts again
/// without reaching cryptsetup; an admitted one is tried on keyslot 0
/// alone, and a wrong one prompts again, without a limit. End of input, or
/// a run of secret-line that fails, halts: the selector never boots
/// without a key that opened keyslot 0, and a console that answers at once
/// would make a prompt that repeats spin. Once keyslot 0 opened, `offer`,
/// the header when this boot's own cap closed PCR 12 and td read it, asks
/// for the reseal; then the mapping opens on keyslot 0 with the recovery
/// key, whatever the reseal did, and the key is dropped, which zeroes it.
fn recover<C: Runner>(
    offer: Option<&Header>,
    partition: &Path,
    cryptsetup: &mut C,
    line: &mut dyn SecretLine,
    reseal: &mut Reseal<'_, C>,
    key_directory: &Path,
    console: &mut dyn FnMut(&str),
) -> Unlock {
    console(ENTRY_KEYS);
    let key = loop {
        let entry = match line.read(KEY_PROMPT) {
            Ok(Entered::Line(entry)) => entry,
            Ok(Entered::Refused) => {
                console("the entry is longer than 256 bytes or holds a newline: enter it again");
                continue;
            }
            Ok(Entered::Eof) => {
                return Unlock::Halt(
                    "end of input at the recovery key prompt; boot is refused without \
                     the recovery key"
                        .into(),
                )
            }
            Err(error) => {
                return Unlock::Halt(format!(
                    "the recovery key prompt failed: {error}; boot is refused without \
                     the recovery key"
                ))
            }
        };
        let parsed = RecoveryKey::parse(entry.as_bytes());
        drop(entry);
        let key = match parsed {
            Ok(key) => key,
            Err(error) => {
                console(&format!("{error}: enter it again"));
                continue;
            }
        };
        let tested = cryptsetup.run(
            &test_args(partition, RECOVERY_SLOT),
            key.passphrase().expose(),
        );
        match tested {
            Ok(()) => break key,
            // Only cryptsetup's wrong-passphrase status is a wrong key; any
            // other failure fails the boot as a failed open does, rather
            // than prompt without end for a key that cannot be tried.
            Err(error) if exit_code(&error) == Some(EXIT_BAD_PASSPHRASE) => console(&format!(
                "the recovery key does not open keyslot 0 ({error}): enter it again"
            )),
            Err(error) => return Unlock::Failed(error),
        }
    };
    console("the recovery key opens keyslot 0");
    match offer {
        Some(header) => {
            console(RESEAL_WARNING);
            console(RESEAL_KEYS);
            if !confirmed(line, console) {
                console(
                    "reseal declined: this boot opens with the recovery key once and the \
                     header is unchanged",
                );
                return open_recovered(partition, key, cryptsetup, key_directory);
            }
            let report = match reseal(header, &key, cryptsetup, console) {
                Resealed::Complete { keyslot } => format!(
                    "reseal complete: keyslot {keyslot} holds a protector sealed to this \
                     boot chain, first released on the next boot; keyslot 0 remains, so \
                     a failure there returns to recovery"
                ),
                Resealed::Stopped { tested } => format!(
                    "reseal stopped ({}): keyslot 0 remains; the next boot releases the \
                     new protector and removes what is left over, or returns to recovery",
                    match tested {
                        Some(keyslot) => format!("keyslot {keyslot} was tested"),
                        None => "no new keyslot was tested".into(),
                    }
                ),
                Resealed::NotRun(reason) => {
                    format!("no reseal: {reason}; the header is unchanged")
                }
            };
            console(&report);
        }
        None => console(
            "no reseal is offered on this boot: it opens with the recovery key once and \
             the header is unchanged",
        ),
    }
    open_recovered(partition, key, cryptsetup, key_directory)
}

/// The mapping opens on keyslot 0 alone with the recovery key that opened
/// it, which is then dropped, so zeroed, and the volume key is taken with
/// its passphrase.
fn open_recovered<C: Runner>(
    partition: &Path,
    key: RecoveryKey,
    cryptsetup: &mut C,
    key_directory: &Path,
) -> Unlock {
    let passphrase = key.passphrase();
    drop(key);
    open(
        partition,
        RECOVERY_SLOT,
        &open_slot_args(partition, MAPPING_NAME, RECOVERY_SLOT),
        passphrase.expose(),
        cryptsetup,
        key_directory,
    )
}

/// The reseal's confirmation: exactly `RESEAL_WORD` on the console, read
/// through secret-line, which discards what was typed before its prompt.
/// Anything else declines, end of input and a failed read included.
fn confirmed(line: &mut dyn SecretLine, console: &mut dyn FnMut(&str)) -> bool {
    match line.read(RESEAL_PROMPT) {
        Ok(Entered::Line(answer)) => answer.as_bytes() == RESEAL_WORD,
        Ok(Entered::Eof | Entered::Refused) => false,
        Err(error) => {
            console(&format!("the reseal question failed: {error}"));
            false
        }
    }
}

/// `args` opens the mapping with `key` on standard input, then the same key
/// yields the volume key. The caller drops `key`, which zeroes it.
fn open<C: Runner>(
    partition: &Path,
    keyslot: u8,
    args: &[OsString],
    key: &[u8],
    cryptsetup: &mut C,
    key_directory: &Path,
) -> Unlock {
    if let Err(error) = cryptsetup.run(args, key) {
        return Unlock::Failed(error);
    }
    match volume_key(partition, key, cryptsetup, key_directory) {
        Ok(volume) => Unlock::Boot {
            keyslot,
            key: volume,
        },
        Err(error) => Unlock::Failed(error),
    }
}

/// cryptsetup writes the volume key into a fresh mode-0700 directory, the
/// key is read and the file unlinked at once, and the directory removed,
/// whether or not each step before succeeded.
fn volume_key<C: Runner>(
    partition: &Path,
    key: &[u8],
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
    let volume = cryptsetup
        .run(&dump_volume_key_args(partition, &file), key)
        .and_then(|()| read_key(&file));
    let unlinked = match fs::remove_file(&file) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    };
    let removed = fs::remove_dir(directory);
    let volume = volume?;
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
    Ok(volume)
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
pub(crate) mod tests {
    use super::*;
    use crate::cap::tests::{capped, read_reply, EXTEND_REPLY};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use td_protector::luks2::HeaderCopy;
    use td_protector::token::{Role, Token};
    use td_protector::Secret;
    use td_tpm::{Client, SealedObject, Transport};

    thread_local! {
        /// Entries not yet dropped, so zeroed, on this thread.
        pub(crate) static LIVE_ENTRIES: Cell<isize> = const { Cell::new(0) };
    }

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// The recovery key's 48 digits, as keyslot 0 holds them.
    const KEY: &[u8] = b"123459123459123459123459123459123459123459123459";
    /// The same key as an operator may type it.
    const TYPED: &str = " 123459-123459 123459-123459  123459-123459-123459 123459 ";
    /// A key the codec admits that keyslot 0 does not hold.
    const WRONG: &str = "000000-000000-000000-000000-000000-000000-000000-000000";

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
    /// and whether the key file's directory existed, fails `open` on
    /// request, and opens keyslot 0 only with `KEY`. `capped`, when set,
    /// must hold at every command.
    struct Script {
        commands: Vec<(Vec<String>, Vec<u8>)>,
        open_fails: bool,
        dump: Dump,
        key: [u8; VOLUME_KEY_BYTES],
        directory_mode: Option<u32>,
        capped: Option<Box<dyn Fn() -> bool>>,
        /// Every keyslot test exits with this status instead.
        test_exit: Option<i32>,
    }

    /// The runner's error for a cryptsetup `open` that exited `code`.
    fn exited(code: i32) -> io::Error {
        use std::os::unix::process::ExitStatusExt;
        td_protector::cryptsetup::Failed::new(
            Path::new("/bin/cryptsetup"),
            "open",
            std::process::ExitStatus::from_raw(code << 8),
        )
        .into_error()
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
                capped: None,
                test_exit: None,
            }
        }
        fn verbs(&self) -> Vec<&str> {
            self.commands
                .iter()
                .map(|(args, _)| args.first().map(String::as_str).unwrap_or(""))
                .collect()
        }
        /// The keyslot tests run, by their keyslot operand.
        fn tests(&self) -> Vec<&str> {
            self.commands
                .iter()
                .filter(|(args, _)| args.iter().any(|a| a == "--test-passphrase"))
                .map(|(args, _)| args[5].as_str())
                .collect()
        }
    }

    impl Runner for Script {
        fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
            if let Some(capped) = &self.capped {
                assert!(capped(), "a cryptsetup command ran before the cap");
            }
            let words: Vec<String> = args
                .iter()
                .map(|arg| arg.to_str().unwrap().to_owned())
                .collect();
            self.commands.push((words.clone(), input.to_vec()));
            let slot = words
                .iter()
                .position(|w| w == "--key-slot")
                .map(|at| words[at + 1].clone());
            let opens_0 = slot.as_deref() == Some("0") && input == KEY;
            match words.first().map(String::as_str) {
                Some("open") if words.iter().any(|w| w == "--test-passphrase") => {
                    match self.test_exit {
                        Some(code) => Err(exited(code)),
                        None if opens_0 => Ok(()),
                        None => Err(exited(EXIT_BAD_PASSPHRASE)),
                    }
                }
                Some("open") if self.open_fails => Err(io::Error::other("open failed")),
                Some("open") if slot.is_some() && !opens_0 => Err(exited(EXIT_BAD_PASSPHRASE)),
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

    /// A released secret's open, as `unlock` makes it.
    fn open_released(script: &mut Script, keys: &Path) -> (Unlock, Vec<u8>) {
        let secret = Secret::generate().unwrap();
        let partition = Path::new(PARTITION);
        let unlock = open(
            partition,
            2,
            &open_args(partition, MAPPING_NAME),
            secret.expose(),
            script,
            keys,
        );
        (unlock, secret.expose().to_vec())
    }

    /// No argument carries eight bytes of `secret`.
    fn assert_not_in_argv(script: &Script, secret: &[u8]) {
        for (args, _) in &script.commands {
            for arg in args {
                assert!(!arg
                    .as_bytes()
                    .windows(8)
                    .any(|w| secret.windows(8).any(|s| s == w)));
            }
        }
    }

    /// A released secret opens the mapping by the held partition's name,
    /// then dumps the key into a fresh mode-0700 directory, both with the
    /// secret on standard input and never in argv; the key is read and
    /// both file and directory are gone.
    #[test]
    fn a_release_opens_the_mapping_then_takes_the_volume_key() {
        let scratch = Scratch::new();
        let mut script = Script::new(Dump::Key);
        let (unlock, secret) = open_released(&mut script, &scratch.keys());
        let Unlock::Boot { keyslot, key } = unlock else {
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
        for (_, input) in &script.commands {
            assert_eq!(input, &secret);
        }
        assert_not_in_argv(&script, &secret);
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
        let Unlock::Failed(error) = open_released(&mut script, &scratch.keys()).0 else {
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
        assert!(matches!(
            open_released(&mut script, &scratch.keys()).0,
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
            let Unlock::Failed(error) = open_released(&mut script, &scratch.keys()).0 else {
                panic!("a bad dump booted");
            };
            assert_eq!(script.verbs(), ["open", "luksDump"], "{error}");
            assert!(!scratch.keys().exists(), "{error}");
        }
    }

    /// The body of production function `name` in this file.
    fn body(name: &str) -> &'static str {
        let source = include_str!("selector_release.rs");
        let (production, _) = source.split_once("\n#[cfg(test)]\n").unwrap();
        production
            .split_once(&format!("\nfn {name}<"))
            .or_else(|| production.split_once(&format!("\npub(crate) fn {name}<")))
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap()
    }

    /// The key file is read and unlinked, and its directory removed, before
    /// any of those steps' errors returns, so no path leaves it behind; a
    /// released secret opens by `open_args` and reaches cryptsetup only as
    /// standard input.
    #[test]
    fn the_key_file_is_read_and_unlinked_at_once() {
        let volume_key = body("volume_key");
        let position = |needle: &str| {
            assert_eq!(volume_key.matches(needle).count(), 1, "{needle}");
            volume_key.find(needle).unwrap()
        };
        let fresh = position(".mode(0o700)\n        .create(directory)");
        let dumped = position(".run(&dump_volume_key_args(partition, &file), key)");
        let read = position(".and_then(|()| read_key(&file));");
        let unlinked = position("let unlinked = match fs::remove_file(&file) {");
        let removed = position("let removed = fs::remove_dir(directory);");
        let first_return = position("let volume = volume?;");
        assert!(fresh < dumped && dumped < read && read < unlinked);
        assert!(unlinked < removed && removed < first_return);
        assert_eq!(volume_key[dumped..first_return].matches('?').count(), 0);
        let open = body("open");
        assert!(open.contains("cryptsetup.run(args, key)"));
        assert!(open.find("cryptsetup.run(args").unwrap() < open.find("volume_key(").unwrap());
        let unlock = body("unlock");
        assert!(unlock.contains(
            "Outcome::Released { keyslot, secret } => open(\n            partition,\n            \
             keyslot,\n            &open_args(partition, MAPPING_NAME),\n            \
             secret.expose(),"
        ));
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
    struct Shared(Rc<RefCell<Answering>>);
    struct Handle(Rc<RefCell<Answering>>);
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

    fn shared(pcr12: [u8; 32]) -> (Shared, Rc<RefCell<Answering>>) {
        let state = Rc::new(RefCell::new(Answering {
            pcr12,
            extended: false,
            commands: 0,
            extends: 0,
        }));
        (Shared(state.clone()), state)
    }

    /// What the operator does at one secret-line prompt.
    #[derive(Clone)]
    enum Typed {
        Line(&'static str),
        Eof,
        Refused,
        Fail,
    }

    /// The console: each read takes the next typed answer, and one past
    /// the last is end of input, so no test can prompt without end.
    struct Console {
        typed: VecDeque<Typed>,
        prompts: Vec<String>,
    }
    impl Console {
        fn new(typed: &[Typed]) -> Self {
            Self {
                typed: typed.iter().cloned().collect(),
                prompts: Vec::new(),
            }
        }
    }
    impl SecretLine for Console {
        fn read(&mut self, prompt: &str) -> io::Result<Entered> {
            self.prompts.push(prompt.to_owned());
            match self.typed.pop_front().unwrap_or(Typed::Eof) {
                Typed::Line(text) => {
                    let mut entry = Entry::new();
                    entry.fill_from(&mut text.as_bytes())?;
                    Ok(Entered::Line(entry))
                }
                Typed::Eof => Ok(Entered::Eof),
                Typed::Refused => Ok(Entered::Refused),
                Typed::Fail => Err(io::Error::other("/bin/secret-line: exit status: 1")),
            }
        }
    }

    /// One boot's unlock with the console's answers; every entry is zeroed
    /// when it returns.
    fn run_unlock<T: Tpm>(
        header: &Result<Header, String>,
        tpm: Option<&mut T>,
        script: &mut Script,
        console: &mut Console,
        scratch: &Scratch,
    ) -> (Unlock, Vec<String>) {
        let mut lines = Vec::new();
        let outcome = unlock(
            header,
            Path::new(PARTITION),
            tpm,
            script,
            console,
            &scratch.keys(),
            &mut |line| lines.push(line.to_owned()),
        );
        assert_eq!(LIVE_ENTRIES.with(Cell::get), 0, "an entry outlived it");
        (outcome, lines)
    }

    /// Each outcome the release reaches without a released secret runs no
    /// cryptsetup and extends the cap at most once: no TPM (no cap), a
    /// first-boot token the TPM refuses (the cap closes, nothing released),
    /// PCR 12 already closed, a refused header, and a TPM that will not
    /// open (an uncertain cap). Recovery then prompts, and end of input
    /// halts it; the uncertain cap halts with no prompt.
    #[test]
    fn every_unreleased_outcome_reaches_recovery_or_halts_before_cryptsetup() {
        let scratch = Scratch::new();
        let with_token = header(vec![first_boot_token()]);
        let end = "end of input at the recovery key prompt";

        let mut script = Script::new(Dump::Key);
        let mut console = Console::new(&[]);
        let (outcome, lines) =
            run_unlock::<Shared>(&with_token, None, &mut script, &mut console, &scratch);
        let Unlock::Halt(reason) = outcome else {
            panic!("no TPM booted")
        };
        assert!(reason.starts_with(end), "{reason}");
        assert!(lines
            .iter()
            .any(|l| l.starts_with("no TPM device: nothing released")));
        assert_eq!(console.prompts, [KEY_PROMPT]);
        assert!(script.commands.is_empty(), "{lines:?}");

        let (mut tpm, state) = shared([0; 32]);
        let mut script = Script::new(Dump::Key);
        let mut console = Console::new(&[]);
        let (outcome, lines) = run_unlock(
            &with_token,
            Some(&mut tpm),
            &mut script,
            &mut console,
            &scratch,
        );
        let Unlock::Halt(reason) = outcome else {
            panic!("a refused token booted")
        };
        assert!(reason.starts_with(end), "{reason}");
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
        assert!(lines.iter().any(|l| l == "no td protector released"));
        assert_eq!(state.borrow().extends, 1);
        assert!(script.commands.is_empty());

        let (mut tpm, state) = shared([1; 32]);
        let mut script = Script::new(Dump::Key);
        let mut console = Console::new(&[]);
        let (outcome, lines) = run_unlock(
            &header(Vec::new()),
            Some(&mut tpm),
            &mut script,
            &mut console,
            &scratch,
        );
        assert!(matches!(outcome, Unlock::Halt(_)));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("PCR 12 release cap: PCR 12 already extended")));
        assert_eq!(state.borrow().extends, 0);
        assert!(script.commands.is_empty());

        let mut tpm = Unopenable(0);
        let mut script = Script::new(Dump::Key);
        let mut console = Console::new(&[Typed::Line(TYPED)]);
        let (outcome, _) = run_unlock(
            &with_token,
            Some(&mut tpm),
            &mut script,
            &mut console,
            &scratch,
        );
        let Unlock::Halt(reason) = outcome else {
            panic!("an uncertain cap booted")
        };
        assert!(reason.contains("platform reset required"), "{reason}");
        assert!(tpm.0 >= 2, "the token and the cap each opened the TPM");
        assert!(console.prompts.is_empty(), "an uncertain cap prompted");
        assert!(script.commands.is_empty());

        // A refused header is still capped, and reaches recovery.
        let (mut tpm, state) = shared([0; 32]);
        let mut script = Script::new(Dump::Key);
        let mut console = Console::new(&[]);
        let (outcome, lines) = run_unlock(
            &Err("no valid copy".into()),
            Some(&mut tpm),
            &mut script,
            &mut console,
            &scratch,
        );
        assert!(matches!(outcome, Unlock::Halt(_)));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("LUKS2 header refused: no valid copy")));
        assert_eq!(state.borrow().extends, 1);
        assert!(script.commands.is_empty());
        assert!(!scratch.keys().exists());
    }

    /// What one run of the recovery flow did.
    struct Recovered {
        unlock: Unlock,
        lines: Vec<String>,
        prompts: Vec<String>,
        /// The passphrase each reseal was given.
        resealed: Vec<Vec<u8>>,
    }

    /// The recovery flow over `typed`, with `offer` and a reseal that
    /// records its key and answers `answer`.
    fn run_recover(
        offer: Option<&Header>,
        typed: &[Typed],
        script: &mut Script,
        scratch: &Scratch,
        answer: Resealed,
    ) -> Recovered {
        let mut console = Console::new(typed);
        let mut lines = Vec::new();
        let mut resealed = Vec::new();
        let mut reseal = |_: &Header,
                          key: &RecoveryKey,
                          cryptsetup: &mut Script,
                          console: &mut dyn FnMut(&str)| {
            // Once keyslot 0 opened, and before the mapping is.
            assert_eq!(cryptsetup.verbs().last(), Some(&"open"));
            assert_eq!(cryptsetup.tests().last(), Some(&"0"));
            assert_eq!(cryptsetup.commands.last().unwrap().1, KEY);
            console("resealing");
            resealed.push(key.passphrase().expose().to_vec());
            answer.clone()
        };
        let unlock = recover(
            offer,
            Path::new(PARTITION),
            script,
            &mut console,
            &mut reseal,
            &scratch.keys(),
            &mut |line| lines.push(line.to_owned()),
        );
        assert_eq!(LIVE_ENTRIES.with(Cell::get), 0, "an entry outlived it");
        Recovered {
            unlock,
            lines,
            prompts: console.prompts,
            resealed,
        }
    }

    /// The recovery key reached cryptsetup only on standard input, tried on
    /// keyslot 0 alone or for the volume key, which `--key-slot` cannot
    /// restrict; it is in no argument and no console line.
    fn assert_key_on_keyslot_0_alone(script: &Script, lines: &[String]) {
        for (args, input) in &script.commands {
            if input == KEY {
                let slot = args.iter().position(|a| a == "--key-slot");
                assert!(
                    slot.is_some_and(|at| args[at + 1] == "0") || args[0] == "luksDump",
                    "{args:?}"
                );
            }
        }
        assert_not_in_argv(script, KEY);
        for line in lines {
            assert!(
                !line.contains("123459") && !line.contains("000000"),
                "{line}"
            );
        }
    }

    /// Refused and malformed entries prompt again without cryptsetup; a
    /// wrong key fails keyslot 0's test and prompts again; the right one,
    /// typed with separators, opens the mapping on keyslot 0 and yields the
    /// volume key. The prompt is the applet's: no console line carries it.
    #[test]
    fn a_wrong_key_prompts_again_and_the_right_one_opens_keyslot_0() {
        let scratch = Scratch::new();
        let mut script = Script::new(Dump::Key);
        let typed = [
            Typed::Line(""),
            Typed::Line("12345"),
            // The first group's check digit is wrong.
            Typed::Line("123450-123459-123459-123459-123459-123459-123459-123459"),
            Typed::Line("123459 123459\t123459"),
            Typed::Refused,
            Typed::Line(WRONG),
            Typed::Line(WRONG),
            Typed::Line(TYPED),
        ];
        let run = run_recover(
            None,
            &typed,
            &mut script,
            &scratch,
            Resealed::Complete { keyslot: 9 },
        );
        let Unlock::Boot { keyslot, key } = run.unlock else {
            panic!("the right key did not boot: {:?}", run.lines);
        };
        assert_eq!(keyslot, 0);
        assert_eq!(key.expose(), script.key);
        assert_eq!(run.prompts, vec![KEY_PROMPT; typed.len()]);
        // Only the two wrong keys and the right one reached cryptsetup.
        assert_eq!(script.tests(), ["0", "0", "0"]);
        assert_eq!(script.verbs(), ["open", "open", "open", "open", "luksDump"]);
        assert_eq!(
            script.commands[3].0,
            [
                "open",
                "--type",
                "luks2",
                "--key-slot",
                "0",
                "--key-file=-",
                PARTITION,
                MAPPING_NAME
            ]
        );
        for (_, input) in &script.commands[2..] {
            assert_eq!(input, KEY);
        }
        assert!(run.resealed.is_empty());
        assert_key_on_keyslot_0_alone(&script, &run.lines);
        assert_eq!(run.lines.first().map(String::as_str), Some(ENTRY_KEYS));
        for expected in [
            "recovery key entry has 0 digits, not 48: enter it again",
            "recovery key entry has 5 digits, not 48: enter it again",
            "recovery key group 1 has a wrong check digit: enter it again",
            "recovery key entry has a character other than a digit, space or hyphen \
             at byte 14: enter it again",
            "the entry is longer than 256 bytes or holds a newline: enter it again",
            "the recovery key opens keyslot 0",
            "no reseal is offered on this boot: it opens with the recovery key once \
             and the header is unchanged",
        ] {
            assert!(
                run.lines.iter().any(|l| l == expected),
                "{expected}: {:?}",
                run.lines
            );
        }
        assert_eq!(
            run.lines
                .iter()
                .filter(|l| l.starts_with("the recovery key does not open keyslot 0"))
                .count(),
            2
        );
        for line in &run.lines {
            assert!(!line.contains(KEY_PROMPT.trim_end()), "{line}");
        }
    }

    /// End of input, before any key or after a wrong one, halts and never
    /// opens the mapping; so does a run of secret-line that fails.
    #[test]
    fn end_of_input_or_a_failed_prompt_halts_without_booting() {
        let scratch = Scratch::new();
        let offered = header(Vec::new()).unwrap();
        for (typed, expected) in [
            (vec![Typed::Eof], "end of input at the recovery key prompt"),
            (
                vec![Typed::Line(WRONG), Typed::Line("1"), Typed::Eof],
                "end of input at the recovery key prompt",
            ),
            (
                vec![Typed::Fail],
                "the recovery key prompt failed: /bin/secret-line: exit status: 1",
            ),
        ] {
            let mut script = Script::new(Dump::Key);
            let run = run_recover(
                Some(&offered),
                &typed,
                &mut script,
                &scratch,
                Resealed::Complete { keyslot: 1 },
            );
            let Unlock::Halt(reason) = run.unlock else {
                panic!("booted without a key");
            };
            assert!(reason.starts_with(expected), "{reason}");
            assert!(reason.ends_with("boot is refused without the recovery key"));
            assert!(script
                .commands
                .iter()
                .all(|(args, _)| args[1] == "--test-passphrase"));
            assert!(run.resealed.is_empty());
            assert!(!run.prompts.iter().any(|p| p == RESEAL_PROMPT));
            assert!(!scratch.keys().exists());
        }
    }

    /// Only cryptsetup's wrong-passphrase status prompts again: any other
    /// failure of keyslot 0's test (a missing keyslot or bad arguments, a
    /// wrong device, a signal) fails the boot at once, as a failed open
    /// does, with no second prompt and nothing opened.
    #[test]
    fn a_keyslot_test_that_fails_otherwise_fails_the_boot() {
        let scratch = Scratch::new();
        for code in [1, 3, 4, 5] {
            let mut script = Script::new(Dump::Key);
            script.test_exit = Some(code);
            let run = run_recover(
                Some(&header(Vec::new()).unwrap()),
                &[Typed::Line(TYPED), Typed::Line(TYPED)],
                &mut script,
                &scratch,
                Resealed::Complete { keyslot: 1 },
            );
            let Unlock::Failed(error) = run.unlock else {
                panic!("exit {code} did not fail the boot: {:?}", run.lines);
            };
            assert_eq!(exit_code(&error), Some(code));
            assert_eq!(run.prompts, [KEY_PROMPT], "exit {code}");
            assert_eq!(script.tests(), ["0"]);
            assert!(run.resealed.is_empty());
            assert!(!scratch.keys().exists());
        }
    }

    /// Confirmed with exactly `reseal`, the reseal runs once keyslot 0
    /// opened, with that key, before the mapping opens; whatever it did,
    /// the volume opens on keyslot 0 with the recovery key.
    #[test]
    fn a_confirmed_reseal_runs_then_the_recovery_key_opens_the_volume() {
        let scratch = Scratch::new();
        let offered = header(Vec::new()).unwrap();
        for (answer, report) in [
            (
                Resealed::Complete { keyslot: 1 },
                "reseal complete: keyslot 1 holds a protector sealed to this boot chain",
            ),
            (
                Resealed::Stopped { tested: Some(1) },
                "reseal stopped (keyslot 1 was tested): keyslot 0 remains",
            ),
            (
                Resealed::Stopped { tested: None },
                "reseal stopped (no new keyslot was tested): keyslot 0 remains",
            ),
            (
                Resealed::NotRun("PCR 9 is unmeasured".into()),
                "no reseal: PCR 9 is unmeasured; the header is unchanged",
            ),
        ] {
            let mut script = Script::new(Dump::Key);
            let run = run_recover(
                Some(&offered),
                &[Typed::Line(TYPED), Typed::Line("reseal")],
                &mut script,
                &scratch,
                answer,
            );
            let Unlock::Boot { keyslot, .. } = run.unlock else {
                panic!("{:?}", run.lines);
            };
            assert_eq!(keyslot, 0);
            assert_eq!(run.prompts, [KEY_PROMPT, RESEAL_PROMPT]);
            let warning = run.lines.iter().position(|l| l == RESEAL_WARNING);
            let keys = run.lines.iter().position(|l| l == RESEAL_KEYS);
            assert!(warning.is_some() && keys == warning.map(|w| w + 1));
            assert_eq!(run.resealed, [KEY.to_vec()]);
            assert!(
                run.lines.iter().any(|l| l.starts_with(report)),
                "{report}: {:?}",
                run.lines
            );
            let resealing = run.lines.iter().position(|l| l == "resealing").unwrap();
            let opened = run
                .lines
                .iter()
                .position(|l| l == "the recovery key opens keyslot 0")
                .unwrap();
            assert!(opened < resealing);
            assert_eq!(script.verbs(), ["open", "open", "luksDump"]);
            assert_key_on_keyslot_0_alone(&script, &run.lines);
        }
    }

    /// Anything but exactly `reseal` declines, end of input and a failed
    /// read included: no reseal runs, and the boot opens once.
    #[test]
    fn anything_but_the_word_declines_the_reseal() {
        let scratch = Scratch::new();
        let offered = header(Vec::new()).unwrap();
        for answer in [
            Typed::Line(""),
            Typed::Line("y"),
            Typed::Line("yes"),
            Typed::Line("Reseal"),
            Typed::Line("reseal "),
            Typed::Line(" reseal"),
            Typed::Line("reseals"),
            Typed::Eof,
            Typed::Refused,
            Typed::Fail,
        ] {
            let mut script = Script::new(Dump::Key);
            let run = run_recover(
                Some(&offered),
                &[Typed::Line(TYPED), answer],
                &mut script,
                &scratch,
                Resealed::Complete { keyslot: 1 },
            );
            assert!(matches!(run.unlock, Unlock::Boot { keyslot: 0, .. }));
            assert!(run.resealed.is_empty());
            assert_eq!(run.prompts, [KEY_PROMPT, RESEAL_PROMPT]);
            assert!(run.lines.iter().any(|l| l == RESEAL_WARNING));
            assert!(run.lines.iter().any(|l| l
                == "reseal declined: this boot opens with the recovery key once and the \
                    header is unchanged"));
            assert_eq!(script.verbs(), ["open", "open", "luksDump"]);
        }
    }

    /// Through the whole unlock: a refused first-boot token reaches
    /// recovery with the reseal offered, every cryptsetup command follows
    /// the cap, and the confirmed reseal runs td-protector's, which this
    /// TPM's refused PCR read stops before any cryptsetup step; the volume
    /// still opens on keyslot 0. Without a TPM, or with PCR 12 already
    /// closed, no reseal is offered.
    #[test]
    fn the_recovery_flow_follows_the_cap_and_offers_the_reseal_only_after_its_own() {
        let scratch = Scratch::new();
        let with_token = header(vec![first_boot_token()]);

        let (mut tpm, state) = shared([0; 32]);
        let mut script = Script::new(Dump::Key);
        let extended = state.clone();
        script.capped = Some(Box::new(move || extended.borrow().extended));
        let mut console = Console::new(&[
            Typed::Line(WRONG),
            Typed::Line(TYPED),
            Typed::Line("reseal"),
        ]);
        let (outcome, lines) = run_unlock(
            &with_token,
            Some(&mut tpm),
            &mut script,
            &mut console,
            &scratch,
        );
        let Unlock::Boot { keyslot, key } = outcome else {
            panic!("{lines:?}");
        };
        assert_eq!((keyslot, key.expose()), (0, &script.key[..]));
        assert_eq!(console.prompts, [KEY_PROMPT, KEY_PROMPT, RESEAL_PROMPT]);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("no reseal: read PCRs 4 and 9: ")),
            "{lines:?}"
        );
        assert_eq!(state.borrow().extends, 1);
        assert_eq!(script.verbs(), ["open", "open", "open", "luksDump"]);
        assert_key_on_keyslot_0_alone(&script, &lines);

        for pcr12 in [None, Some([1; 32])] {
            let mut script = Script::new(Dump::Key);
            let mut console = Console::new(&[Typed::Line(TYPED), Typed::Line("reseal")]);
            let (outcome, lines) = match pcr12 {
                None => {
                    run_unlock::<Shared>(&with_token, None, &mut script, &mut console, &scratch)
                }
                Some(pcr12) => {
                    let (mut tpm, state) = shared(pcr12);
                    let run = run_unlock(
                        &with_token,
                        Some(&mut tpm),
                        &mut script,
                        &mut console,
                        &scratch,
                    );
                    assert_eq!(state.borrow().extends, 0);
                    run
                }
            };
            assert!(
                matches!(outcome, Unlock::Boot { keyslot: 0, .. }),
                "{lines:?}"
            );
            assert_eq!(console.prompts, [KEY_PROMPT]);
            assert_eq!(script.verbs(), ["open", "open", "luksDump"]);
        }
    }

    /// The prompts are printable ASCII within secret-line's 128-byte
    /// bound, and td-boot reads its statuses and line bound as td-init
    /// states them; system-x86-64's initramfs test holds the two sources
    /// together, so this crate reads no td-init source.
    #[test]
    fn the_prompts_and_statuses_are_secret_lines() {
        assert_eq!((ENTRY_MAX, EXIT_EOF, EXIT_REFUSED), (256, 3, 4));
        assert_eq!(td_protector::recovery::MAX_ENTRY_LEN, ENTRY_MAX);
        for prompt in [KEY_PROMPT, RESEAL_PROMPT] {
            assert!((1..=128).contains(&prompt.len()), "{prompt}");
            assert!(
                prompt.bytes().all(|b| (b' '..=b'~').contains(&b)),
                "{prompt}"
            );
        }
        assert_eq!(RESEAL_WORD, b"reseal");
        assert!(RESEAL_PROMPT.contains("Type reseal"));
        for named in ["48 digits", "top row", "unshifted", "Num Lock"] {
            assert!(ENTRY_KEYS.contains(named), "{named}");
        }
        assert!(RESEAL_KEYS.contains("US key positions"));
    }

    /// td-boot never prints a prompt: each is only secret-line's operand.
    /// The recovery flow reaches cryptsetup only by keyslot 0's test and
    /// open and the volume-key dump, never by `open_args`, which tries
    /// every keyslot; and its key reaches the reseal only once keyslot 0
    /// opened.
    #[test]
    fn the_applet_prints_the_prompts_and_the_key_meets_keyslot_0_alone() {
        let source = include_str!("selector_release.rs");
        let (production, _) = source.split_once("\n#[cfg(test)]\n").unwrap();
        assert_eq!(production.matches("line.read(KEY_PROMPT)").count(), 1);
        assert_eq!(production.matches("line.read(RESEAL_PROMPT)").count(), 1);
        assert_eq!(production.matches("KEY_PROMPT").count(), 2);
        assert_eq!(production.matches("RESEAL_PROMPT").count(), 2);
        assert!(!production.contains("writeln!") && !production.contains("print!"));
        let recover = body("recover");
        assert!(!recover.contains("open_args("));
        assert_eq!(recover.matches("cryptsetup.run(").count(), 1);
        assert!(recover.contains(
            "&test_args(partition, RECOVERY_SLOT),\n            key.passphrase().expose(),"
        ));
        assert!(recover
            .contains("Err(error) if exit_code(&error) == Some(EXIT_BAD_PASSPHRASE) => console("));
        assert!(recover.contains("Err(error) => return Unlock::Failed(error),"));
        let opened = body("open_recovered");
        assert!(!opened.contains("open_args("));
        assert!(opened.contains(
            "&open_slot_args(partition, MAPPING_NAME, RECOVERY_SLOT),\n        \
             passphrase.expose(),"
        ));
        let tested = recover.find("Ok(()) => break key,").unwrap();
        assert!(
            tested
                < recover
                    .find("reseal(header, &key, cryptsetup, console)")
                    .unwrap()
        );
        // The reseal runs before the open, so a completed reseal survives
        // a failed open.
        assert!(
            recover
                .find("reseal(header, &key, cryptsetup, console)")
                .unwrap()
                < recover.rfind("open_recovered(").unwrap()
        );
        assert_eq!(recover.matches("open_recovered(").count(), 2);
        assert!(tested < recover.find("open_recovered(").unwrap());
        assert_eq!(RECOVERY_SLOT, 0);
        // Release, with its cap, before any recovery.
        let unlock = body("unlock");
        let released = unlock
            .find("release::release(header, partition, tpm.as_deref_mut(), cryptsetup, console)")
            .unwrap();
        assert!(released < unlock.find("recover(").unwrap());
        assert!(unlock
            .contains("let offer = if reseal_offerable {\n                header.as_ref().ok()"));
        // The applet's environment is cleared and its stdout is the pipe.
        let applet = production
            .split_once("impl SecretLine for Applet {")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        for needle in [".arg(prompt)", ".env_clear()", ".stdout(Stdio::piped())"] {
            assert!(applet.contains(needle), "{needle}");
        }
    }

    /// A stand-in applet in the scratch directory: it records its argv and
    /// environment, then runs `body`.
    fn stand_in(scratch: &Scratch, name: &str, body: &str) -> Applet {
        let program = scratch.0.join(name);
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$@\" > '{dir}/{name}.argv'\n\
             export -p > '{dir}/{name}.env'\n{body}\n",
            dir = scratch.0.display()
        );
        // A child shell writes it, so no descriptor open for writing here
        // reaches a child a sibling test spawns (ETXTBSY).
        let written = Command::new("/bin/sh")
            .args([
                "-c",
                "printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"",
                "sh",
            ])
            .arg(&program)
            .arg(script)
            .status()
            .unwrap();
        assert!(written.success());
        Applet(program)
    }

    /// The applet's statuses: 0 with its line, 3 end of input, 4 refused,
    /// anything else an error naming it; its prompt is its one argument
    /// and its environment is empty; output beyond `ENTRY_MAX` is refused.
    #[test]
    fn the_applet_runs_once_per_entry_and_maps_its_statuses() {
        let scratch = Scratch::new();
        let mut line = stand_in(&scratch, "line", "printf '%s' '123459-123459'\nexit 0");
        let Entered::Line(entry) = line.read(KEY_PROMPT).unwrap() else {
            panic!("no line");
        };
        assert_eq!(entry.as_bytes(), b"123459-123459");
        drop(entry);
        assert_eq!(
            fs::read_to_string(scratch.0.join("line.argv")).unwrap(),
            format!("1\n{KEY_PROMPT}\n")
        );
        let env = fs::read_to_string(scratch.0.join("line.env")).unwrap();
        assert!(!env.contains("HOME=") && !env.contains("USER="), "{env}");

        let Entered::Line(entry) = stand_in(&scratch, "empty", "exit 0")
            .read(RESEAL_PROMPT)
            .unwrap()
        else {
            panic!("no line");
        };
        assert!(entry.as_bytes().is_empty());
        drop(entry);
        let full = format!("printf '%s' '{}'\nexit 0", "1".repeat(256));
        let Entered::Line(entry) = stand_in(&scratch, "full", &full).read(KEY_PROMPT).unwrap()
        else {
            panic!("no line");
        };
        assert_eq!(entry.as_bytes().len(), 256);
        drop(entry);

        assert!(matches!(
            stand_in(&scratch, "eof", "exit 3")
                .read(KEY_PROMPT)
                .unwrap(),
            Entered::Eof
        ));
        assert!(matches!(
            stand_in(&scratch, "refused", "exit 4")
                .read(KEY_PROMPT)
                .unwrap(),
            Entered::Refused
        ));
        for (name, body) in [
            ("failed", "exit 1".to_string()),
            ("other", "exit 2".to_string()),
            ("killed", "kill -9 $$".to_string()),
            ("long", format!("printf '%s' '{}'\nexit 0", "1".repeat(257))),
        ] {
            let error = match stand_in(&scratch, name, &body).read(KEY_PROMPT) {
                Err(error) => error,
                Ok(_) => panic!("{name} was admitted"),
            };
            assert!(error.to_string().contains(name), "{name}: {error}");
        }
        let error = match Applet(scratch.0.join("absent")).read(KEY_PROMPT) {
            Err(error) => error,
            Ok(_) => panic!("an absent applet ran"),
        };
        assert!(error.to_string().contains("absent"), "{error}");
        assert_eq!(LIVE_ENTRIES.with(Cell::get), 0);
    }
}
