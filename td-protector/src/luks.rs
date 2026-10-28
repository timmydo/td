//! The td-luks runner the installer and the selector share (DESIGN.md
//! "LUKS runner"): each command's exact arguments, and one child per
//! command with key material by descriptor only.
//!
//! Every child gets a cleared environment and an absolute program. The
//! first secret goes on standard input; the second, the new key `add-key`
//! reads as its key file, through a std pipe this process keeps open and
//! names as `/proc/<pid>/fd/N`. No argv element, environment variable or file
//! carries key material, and nothing here is `unsafe`.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The most a child's input may be: one page, which any pipe holds, so
/// filling it never blocks. A secret is at most 48 bytes, and a token's JSON
/// is held to `token::MAX_TOKEN_JSON`, no more than this.
pub const MAX_PIPE_INPUT: usize = 4096;

/// The shortest key for either destructive keyslot operation: td's
/// protector secret. Refuse less before starting a child.
pub const MIN_KILL_KEY: usize = crate::SECRET_LEN;

/// The most standard output the runner reads from one child, the metadata
/// dump's bound; a child writing more fails.
pub const MAX_OUTPUT: usize = crate::luks2::MAX_METADATA_JSON;

/// td-luks's inactive mapping status. Active is 0; any other exit is an error.
pub const STATUS_INACTIVE: i32 = 4;

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Format the fixed td profile, with keyslot 0's passphrase on stdin.
pub fn format_args(uuid: &str, label: &str, device: &Path) -> Vec<OsString> {
    vec!["format".into(), uuid.into(), label.into(), device.into()]
}

/// Authorize with BY on stdin; read the new key from its held descriptor.
pub fn add_key_args(device: &Path, by: u8, new_slot: u8, new_key: &Path) -> Vec<OsString> {
    vec![
        "add-key".into(),
        by.to_string().into(),
        new_slot.to_string().into(),
        device.into(),
        new_key.into(),
    ]
}

pub fn token_import_args(device: &Path, id: Option<u8>) -> Vec<OsString> {
    let mut args = os(&["token-import"]);
    if let Some(id) = id {
        args.push(id.to_string().into());
    }
    args.push(device.into());
    args
}

pub fn token_replace_args(device: &Path, id: u8) -> Vec<OsString> {
    vec!["token-replace".into(), id.to_string().into(), device.into()]
}

pub fn token_remove_args(device: &Path, id: u8) -> Vec<OsString> {
    vec!["token-remove".into(), id.to_string().into(), device.into()]
}

pub fn kill_slot_args(device: &Path, slot: u8) -> Vec<OsString> {
    vec!["kill-slot".into(), slot.to_string().into(), device.into()]
}

/// Authenticate a bound key, sanitize freed areas and retire unbound slots.
pub fn kill_unbound_args(device: &Path) -> Vec<OsString> {
    vec!["kill-unbound".into(), device.into()]
}

pub fn dump_metadata_args(device: &Path) -> Vec<OsString> {
    vec!["metadata".into(), device.into()]
}

/// Create FILE exclusively at mode 0400; key bytes never reach stdout.
pub fn dump_volume_key_args(device: &Path, file: &Path) -> Vec<OsString> {
    vec!["volume-key".into(), device.into(), file.into()]
}

pub fn open_args(device: &Path, name: &str) -> Vec<OsString> {
    vec!["open".into(), device.into(), name.into()]
}

pub fn open_slot_args(device: &Path, name: &str, slot: u8) -> Vec<OsString> {
    vec![
        "open".into(),
        slot.to_string().into(),
        device.into(),
        name.into(),
    ]
}

pub fn open_by_volume_key_args(device: &Path, name: &str, key: &Path) -> Vec<OsString> {
    vec![
        "open-volume-key".into(),
        device.into(),
        name.into(),
        key.into(),
    ]
}

pub fn close_args(name: &str) -> Vec<OsString> {
    os(&["close", name])
}

pub fn status_args(name: &str) -> Vec<OsString> {
    os(&["status", name])
}

pub fn test_args(device: &Path, slot: u8) -> Vec<OsString> {
    vec!["test".into(), slot.to_string().into(), device.into()]
}

/// Test only the copied header and keyslot area; DEVICE is never mutated.
pub fn detached_test_args(header: &Path, device: &Path, slot: u8) -> Vec<OsString> {
    vec![
        "test".into(),
        slot.to_string().into(),
        device.into(),
        header.into(),
    ]
}

/// The longest progress line read; a longer one is skipped as unreadable.
pub const MAX_PROGRESS_LINE: usize = 4096;

/// One line of a reencryption's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// `device_bytes` of `device_size` rewritten.
    Bytes { done: u64, size: u64 },
    /// A line that is not cryptsetup's progress object.
    Unreadable,
}

/// cryptsetup 2.8.8's progress object, `{"device":…,"device_bytes":"N",
/// "device_size":"M",…}`, its two counts as decimal strings.
pub fn progress_line(line: &[u8]) -> Progress {
    let field = |name: &str| -> Option<u64> {
        let key = format!("\"{name}\":\"");
        let at = line
            .windows(key.len())
            .position(|window| window == key.as_bytes())?;
        let rest = line.get(at + key.len()..)?;
        let end = rest.iter().position(|byte| *byte == b'"')?;
        let digits = rest.get(..end)?;
        if digits.is_empty() || digits.len() > 20 || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse().ok()
    };
    if line.first() != Some(&b'{') || line.last() != Some(&b'}') {
        return Progress::Unreadable;
    }
    match (field("device_bytes"), field("device_size")) {
        (Some(done), Some(size)) => Progress::Bytes { done, size },
        _ => Progress::Unreadable,
    }
}

/// How a reencryption protects the hotzone it is rewriting, both in the
/// keyslots area (ENCRYPTION.md "Re-encrypting upgrade").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resilience {
    /// A checksum of each sector, relying on whole atomic 4096-byte writes.
    Checksum,
    /// The data written twice.
    Journal,
}

/// Rotate the volume key under the already active mapping.
pub fn reencrypt_args(
    device: &Path,
    name: &str,
    slot: u8,
    resilience: Resilience,
) -> Vec<OsString> {
    let mode = match resilience {
        Resilience::Checksum => "checksum",
        Resilience::Journal => "journal",
    };
    vec![
        "reencrypt".into(),
        slot.to_string().into(),
        mode.into(),
        device.into(),
        name.into(),
    ]
}

pub fn reencrypt_resume_args(device: &Path, name: &str) -> Vec<OsString> {
    vec!["reencrypt-resume".into(), device.into(), name.into()]
}

/// How long a bounded run sleeps between looks at its child.
const BOUNDED_POLL: Duration = Duration::from_millis(10);

/// How a bounded run ended.
#[derive(Debug)]
pub enum Bounded {
    Exited(ExitStatus),
    /// The deadline passed first: the child was killed and reaped.
    Expired,
    /// The caller was abandoned first: the child was killed and reaped.
    Abandoned,
}

/// What `status` said of a mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mapping {
    Active,
    Inactive,
}

/// Each line `reader` yields, as `progress_line` reads it, until end of
/// file: one over `MAX_PROGRESS_LINE` bytes is unreadable and its rest
/// discarded, and an empty one is skipped.
fn stream_lines(mut reader: impl Read, progress: &mut dyn FnMut(Progress)) -> io::Result<()> {
    let mut line = Vec::with_capacity(MAX_PROGRESS_LINE);
    let mut over = false;
    let mut buffer = [0u8; 512];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        for byte in buffer.get(..read).unwrap_or_default() {
            if *byte != b'\n' {
                if line.len() < MAX_PROGRESS_LINE {
                    line.push(*byte);
                } else {
                    over = true;
                }
                continue;
            }
            if over {
                progress(Progress::Unreadable);
            } else if !line.is_empty() {
                progress(progress_line(&line));
            }
            line.clear();
            over = false;
        }
    }
    if over || !line.is_empty() {
        progress(Progress::Unreadable);
    }
    Ok(())
}

/// Input written whole into a pipe whose write end is closed: a reader gets
/// exactly those bytes and end of file. Anything over `MAX_PIPE_INPUT` is
/// refused rather than risk a write that blocks.
fn filled_pipe(bytes: &[u8]) -> io::Result<io::PipeReader> {
    if bytes.len() > MAX_PIPE_INPUT {
        return Err(invalid(format!(
            "a {}-byte input is over the {MAX_PIPE_INPUT} bytes a pipe is sure to hold",
            bytes.len()
        )));
    }
    let (reader, mut writer) = io::pipe()?;
    writer.write_all(bytes)?;
    drop(writer);
    Ok(reader)
}

/// A key file only a descriptor holds: this process keeps the read end,
/// close-on-exec, and the child opens it by its `/proc` name. The name is
/// a descriptor number and carries no secret.
pub struct KeyFile {
    _held: io::PipeReader,
    path: PathBuf,
}

impl KeyFile {
    pub fn new(secret: &[u8]) -> io::Result<Self> {
        let held = filled_pipe(secret)?;
        let path = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            held.as_raw_fd()
        ));
        Ok(Self { _held: held, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// cryptsetup's exit status when the passphrase opens no keyslot it was
/// let try: 2.8.8's `translate_errno` maps `-EPERM`, the keyslot code's
/// answer to a wrong passphrase, to 2. Every other failure exits otherwise
/// (1 for a missing keyslot or bad arguments, 4 for a wrong device).
pub const EXIT_BAD_PASSPHRASE: i32 = 2;

/// A command that ran and exited unsuccessfully: the program, the verb and
/// the exit status, never an argument.
#[derive(Debug)]
pub struct Failed {
    program: PathBuf,
    verb: String,
    status: ExitStatus,
}

impl Failed {
    pub fn new(program: &Path, verb: &str, status: ExitStatus) -> Self {
        Self {
            program: program.to_path_buf(),
            verb: verb.to_owned(),
            status,
        }
    }

    pub fn into_error(self) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, self)
    }
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} failed ({})",
            self.program.display(),
            self.verb,
            self.status
        )
    }
}

impl std::error::Error for Failed {}

/// The exit code of a command `error` reports as having run and failed;
/// `None` for any other error, such as one that kept the child from
/// starting, and for a child killed by a signal.
pub fn exit_code(error: &io::Error) -> Option<i32> {
    error.get_ref()?.downcast_ref::<Failed>()?.status.code()
}

/// A static td-luks at an absolute path.
pub struct Luks {
    program: PathBuf,
}

impl Luks {
    /// The program is checked at each run: a path that is not absolute is
    /// refused rather than looked up.
    pub fn new(program: PathBuf) -> Self {
        Self { program }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Runs one td-luks command with `input` on its standard input. Its
    /// output is captured, copied to standard error, which it inherits, and
    /// zeroed: no verb that prints key material may run through it.
    pub fn run(&self, args: &[OsString], input: &[u8]) -> io::Result<()> {
        let (status, mut output) = self.child(args, input)?;
        let _ = io::stderr().write_all(&output);
        td_tpm::zero(&mut output);
        if !status.success() {
            return Err(self.failed(args, status));
        }
        Ok(())
    }

    /// Runs one td-luks command with `input` on its standard input,
    /// keeping none of its output, until it exits, `deadline` passes or
    /// `abandoned` says the caller is gone. Past either of those the child
    /// is killed and reaped, so no child outlives the run holding its
    /// input, however long its own work or a lock it waits on would last.
    pub fn run_bounded(
        &self,
        args: &[OsString],
        input: &[u8],
        deadline: Instant,
        abandoned: &mut dyn FnMut() -> bool,
    ) -> io::Result<Bounded> {
        self.checked(args, input)?;
        let stdin = filled_pipe(input)?;
        let mut child = Command::new(&self.program)
            .args(args)
            .env_clear()
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| invalid(format!("cannot run {}: {error}", self.program.display())))?;
        loop {
            let ended = match child.try_wait() {
                Ok(Some(status)) => return Ok(Bounded::Exited(status)),
                Ok(None) if Instant::now() >= deadline => Ok(Bounded::Expired),
                Ok(None) if abandoned() => Ok(Bounded::Abandoned),
                Ok(None) => {
                    std::thread::sleep(BOUNDED_POLL);
                    continue;
                }
                Err(error) => Err(error),
            };
            // A child that exited meanwhile refuses the kill; the wait
            // reaps it either way.
            let _ = child.kill();
            child.wait()?;
            return ended;
        }
    }

    /// Runs a reencryption with `input` on its standard input, passing
    /// each line of its standard output to `progress` as it reads it, each
    /// bounded at `MAX_PROGRESS_LINE` bytes with no total bound; an empty
    /// line, which an interrupted run prints, is skipped. Its exit alone
    /// decides the run.
    pub fn reencrypt(
        &self,
        args: &[OsString],
        input: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> io::Result<()> {
        self.checked(args, input)?;
        let stdin = filled_pipe(input)?;
        let mut child = Command::new(&self.program)
            .args(args)
            .env_clear()
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| invalid(format!("cannot run {}: {error}", self.program.display())))?;
        let read = match child.stdout.take() {
            Some(stdout) => stream_lines(stdout, progress),
            None => Ok(()),
        };
        let status = child.wait()?;
        read?;
        if !status.success() {
            return Err(self.failed(args, status));
        }
        Ok(())
    }

    /// The JSON metadata of the header copy td-luks uses on `device`,
    /// returned rather than copied to standard error.
    pub fn metadata(&self, device: &Path) -> io::Result<Vec<u8>> {
        let args = dump_metadata_args(device);
        let (status, output) = self.child(&args, &[])?;
        if !status.success() {
            return Err(self.failed(&args, status));
        }
        Ok(output)
    }

    /// Whether the mapping `name` is active, by `status`'s exit alone; an
    /// exit meaning neither is an error.
    pub fn mapping(&self, name: &str) -> io::Result<Mapping> {
        let args = status_args(name);
        let (status, mut output) = self.child(&args, &[])?;
        let _ = io::stderr().write_all(&output);
        td_tpm::zero(&mut output);
        match status.code() {
            Some(0) => Ok(Mapping::Active),
            Some(STATUS_INACTIVE) => Ok(Mapping::Inactive),
            _ => Err(self.failed(&args, status)),
        }
    }

    fn failed(&self, args: &[OsString], status: ExitStatus) -> io::Error {
        let verb = args
            .first()
            .map(|verb| verb.to_string_lossy().into_owned())
            .unwrap_or_default();
        Failed::new(&self.program, &verb, status).into_error()
    }

    /// One child: its exit and at most `MAX_OUTPUT` bytes of its standard
    /// output.
    fn child(&self, args: &[OsString], input: &[u8]) -> io::Result<(ExitStatus, Vec<u8>)> {
        self.checked(args, input)?;
        let stdin = filled_pipe(input)?;
        let mut child = Command::new(&self.program)
            .args(args)
            .env_clear()
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| invalid(format!("cannot run {}: {error}", self.program.display())))?;
        let mut output = Vec::new();
        let read = match child.stdout.take() {
            Some(stdout) => stdout
                .take(MAX_OUTPUT as u64 + 1)
                .read_to_end(&mut output)
                .map(|_| ()),
            None => Ok(()),
        };
        // The pipe is closed: a child still writing gets EPIPE.
        let status = child.wait()?;
        read?;
        if output.len() > MAX_OUTPUT {
            td_tpm::zero(&mut output);
            return Err(invalid(format!(
                "{} wrote over {MAX_OUTPUT} bytes",
                self.program.display()
            )));
        }
        Ok((status, output))
    }

    /// The refusals every child passes before it starts.
    fn checked(&self, args: &[OsString], input: &[u8]) -> io::Result<()> {
        if !self.program.is_absolute() {
            return Err(invalid(format!(
                "cannot run {}: not an absolute path",
                self.program.display()
            )));
        }
        if args
            .first()
            .is_some_and(|verb| verb == "kill-slot" || verb == "kill-unbound")
            && input.len() < MIN_KILL_KEY
        {
            return Err(invalid(format!(
                "slot removal needs a key of at least {MIN_KILL_KEY} bytes on standard input, not {}", input.len()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{Role, Token, MAX_PRIVATE_BYTES, MAX_PUBLIC_BYTES, MAX_TOKEN_JSON};
    use std::fs::File;
    use std::io::Read;
    use td_tpm::SealedObject;

    const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
    const NAME: &str = "td-install-0707070707070707";

    fn words(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    #[test]
    fn each_command_is_spelled_exactly() {
        let d = Path::new("/dev/loop7");
        let k = Path::new("/proc/9/fd/4");
        let commands = [
            (
                format_args(UUID, "td-system", d),
                vec!["format", UUID, "td-system", "/dev/loop7"],
            ),
            (
                add_key_args(d, 1, 12, k),
                vec!["add-key", "1", "12", "/dev/loop7", "/proc/9/fd/4"],
            ),
            (
                token_import_args(d, None),
                vec!["token-import", "/dev/loop7"],
            ),
            (
                token_import_args(d, Some(3)),
                vec!["token-import", "3", "/dev/loop7"],
            ),
            (
                token_replace_args(d, 5),
                vec!["token-replace", "5", "/dev/loop7"],
            ),
            (
                token_remove_args(d, 3),
                vec!["token-remove", "3", "/dev/loop7"],
            ),
            (kill_slot_args(d, 2), vec!["kill-slot", "2", "/dev/loop7"]),
            (kill_unbound_args(d), vec!["kill-unbound", "/dev/loop7"]),
            (dump_metadata_args(d), vec!["metadata", "/dev/loop7"]),
            (
                dump_volume_key_args(d, Path::new("/run/key")),
                vec!["volume-key", "/dev/loop7", "/run/key"],
            ),
            (open_args(d, NAME), vec!["open", "/dev/loop7", NAME]),
            (
                open_slot_args(d, NAME, 31),
                vec!["open", "31", "/dev/loop7", NAME],
            ),
            (
                open_by_volume_key_args(d, NAME, k),
                vec!["open-volume-key", "/dev/loop7", NAME, "/proc/9/fd/4"],
            ),
            (close_args(NAME), vec!["close", NAME]),
            (status_args(NAME), vec!["status", NAME]),
            (test_args(d, 31), vec!["test", "31", "/dev/loop7"]),
            (
                detached_test_args(Path::new("/run/header"), d, 31),
                vec!["test", "31", "/dev/loop7", "/run/header"],
            ),
            (
                reencrypt_args(d, NAME, 0, Resilience::Checksum),
                vec!["reencrypt", "0", "checksum", "/dev/loop7", NAME],
            ),
            (
                reencrypt_args(d, NAME, 17, Resilience::Journal),
                vec!["reencrypt", "17", "journal", "/dev/loop7", NAME],
            ),
            (
                reencrypt_resume_args(d, NAME),
                vec!["reencrypt-resume", "/dev/loop7", NAME],
            ),
        ];
        for (actual, expected) in commands {
            assert_eq!(words(&actual), expected);
        }
    }

    /// cryptsetup 2.8.8's progress lines, as the pinned binary printed them
    /// on a file, and what is not one.
    #[test]
    fn a_progress_line_is_read_or_refused() {
        let line = br#"{"device":"/proc/1/fd/5","device_bytes":"276824064","device_size":"536870912","speed":"129815762","eta_ms":"2000","time_ms":"1000"}"#;
        assert_eq!(
            progress_line(line),
            Progress::Bytes {
                done: 276824064,
                size: 536870912
            }
        );
        for bad in [
            &br#"Reencryption interrupted."#[..],
            br#"{"device_bytes":"1"}"#,
            br#"{"device_bytes":"","device_size":"2"}"#,
            br#"{"device_bytes":"-1","device_size":"2"}"#,
            br#"{"device_bytes":"1","device_size":"999999999999999999999"}"#,
            br#"{"device_bytes":1,"device_size":2}"#,
            br#""device_bytes":"1","device_size":"2""#,
        ] {
            assert_eq!(
                progress_line(bad),
                Progress::Unreadable,
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// A reencryption's run streams each line as it comes, skips empty
    /// ones, refuses one over the bound and a last line without its
    /// newline, and fails on the child's exit alone.
    #[test]
    fn a_reencryption_streams_its_progress() {
        let dir = td_fs::TempDir::new("td-protector-reencrypt-progress").unwrap();
        let program = dir.path().join("cryptsetup");
        let long = "x".repeat(MAX_PROGRESS_LINE + 1);
        let body = format!(
            "#!/bin/sh\nread key\n[ \"$key\" = secret ] || exit 9\n\
             echo '{{\"device_bytes\":\"1\",\"device_size\":\"4\"}}'\necho\n\
             echo '{long}'\necho 'Reencryption interrupted.'\n\
             echo '{{\"device_bytes\":\"4\",\"device_size\":\"4\"}}'\nprintf tail\n\
             exit \"$1\"\n"
        );
        let written = Command::new("/bin/sh")
            .args([
                "-c",
                "printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"",
                "sh",
            ])
            .arg(&program)
            .arg(body)
            .status()
            .unwrap();
        assert!(written.success());
        let cryptsetup = Luks::new(program);
        let mut seen = Vec::new();
        cryptsetup
            .reencrypt(&os(&["0"]), b"secret\n", &mut |line| seen.push(line))
            .unwrap();
        assert_eq!(
            seen,
            [
                Progress::Bytes { done: 1, size: 4 },
                Progress::Unreadable,
                Progress::Unreadable,
                Progress::Bytes { done: 4, size: 4 },
                Progress::Unreadable,
            ]
        );
        let error = cryptsetup
            .reencrypt(&os(&["2"]), b"secret\n", &mut |_| {})
            .unwrap_err();
        assert_eq!(exit_code(&error), Some(2), "{error}");
        // A whole disk's lines run past `run`'s output bound: the stream
        // has none.
        let many = dir.path().join("many");
        let lines = MAX_OUTPUT / 40 + 1000;
        let body = format!(
            "#!/bin/sh\ni=0\nwhile [ $i -lt {lines} ]; do\n\
             echo \"{{\\\"device_bytes\\\":\\\"$i\\\",\\\"device_size\\\":\\\"{lines}\\\"}}\"\n\
             i=$((i+1))\ndone\n"
        );
        let written = Command::new("/bin/sh")
            .args([
                "-c",
                "printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"",
                "sh",
            ])
            .arg(&many)
            .arg(body)
            .status()
            .unwrap();
        assert!(written.success());
        let mut count = 0u64;
        let mut last = None;
        Luks::new(many)
            .reencrypt(&os(&[]), b"", &mut |line| {
                if let Progress::Bytes { done, .. } = line {
                    count += 1;
                    last = Some(done);
                }
            })
            .unwrap();
        assert_eq!(count, lines as u64);
        assert_eq!(last, Some(lines as u64 - 1));
    }

    /// The key file holds exactly the secret and then end of file, is open
    /// in this process only, close-on-exec, and is named by this process's
    /// own descriptor table.
    #[test]
    fn a_key_file_is_a_descriptor_holding_exactly_the_secret() {
        let secret = [0x5c; 32];
        let key = KeyFile::new(&secret).unwrap();
        let prefix = format!("/proc/{}/fd/", std::process::id());
        let fd = key.path().to_str().unwrap().strip_prefix(&prefix).unwrap();
        assert!(fd.parse::<u32>().is_ok(), "{fd}");
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap();
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .map(|flags| u32::from_str_radix(flags.trim(), 8).unwrap())
            .unwrap();
        assert_ne!(flags & 0o2_000_000, 0, "not close-on-exec");
        let mut read = Vec::new();
        File::open(key.path())
            .unwrap()
            .read_to_end(&mut read)
            .unwrap();
        assert_eq!(read, secret);
        drop(key);
    }

    /// Input over a page is refused before any pipe is filled, so a fill
    /// can never block; the largest token the format admits is under it.
    #[test]
    fn pipe_input_is_bounded_by_a_page() {
        assert!(filled_pipe(&[0; MAX_PIPE_INPUT]).is_ok());
        let error = filled_pipe(&[0; MAX_PIPE_INPUT + 1]).unwrap_err();
        assert!(error.to_string().contains("pipe"), "{error}");
        const { assert!(MAX_TOKEN_JSON <= MAX_PIPE_INPUT) };
        let largest = Token::new(
            1,
            Role::FirstBoot,
            SealedObject {
                public: vec![0xff; MAX_PUBLIC_BYTES],
                private: vec![0xff; MAX_PRIVATE_BYTES],
            },
        )
        .unwrap();
        assert!(largest.encode().len() <= MAX_TOKEN_JSON);
    }

    /// A program on `PATH`, for a stand-in that runs with no environment.
    fn tool(name: &str) -> PathBuf {
        std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(name))
                    .find(|candidate| candidate.is_file())
            })
            .unwrap()
    }

    /// A cryptsetup stand-in in a fresh directory: each call's argv,
    /// exported environment, standard input and any `/proc` key file it
    /// names, numbered in order. A `code.N` file makes call N exit with the
    /// code it holds.
    struct Recorder {
        dir: td_fs::TempDir,
        cryptsetup: Luks,
    }

    impl Recorder {
        fn new() -> Self {
            let dir = td_fs::TempDir::new("td-protector-luks").unwrap();
            let cat = tool("cat");
            let program = dir.join("cryptsetup");
            let body = format!(
                "#!/bin/sh\nd='{dir}'\nn=0\nif [ -f \"$d/count\" ]; then read n < \"$d/count\"; fi\n\
                 n=$((n+1))\necho \"$n\" > \"$d/count\"\nprintf '%s\\n' \"$@\" > \"$d/argv.$n\"\n\
                 export -p > \"$d/env.$n\"\n'{cat}' > \"$d/stdin.$n\"\nfor last; do :; done\n\
                 case \"$last\" in /proc/*) '{cat}' \"$last\" > \"$d/key.$n\";; esac\n\
                 if [ -f \"$d/code.$n\" ]; then read c < \"$d/code.$n\"; exit \"$c\"; fi\n\
                 echo out-$n\n",
                dir = dir.display(),
                cat = cat.display()
            );
            // A child shell writes it: a descriptor open for writing here
            // would be copied into a child a sibling test is spawning, and
            // running the script would fail with ETXTBSY until it execs.
            let written = Command::new("/bin/sh")
                .args([
                    "-c",
                    "printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"",
                    "sh",
                ])
                .arg(&program)
                .arg(body)
                .status()
                .unwrap();
            assert!(written.success());
            Self {
                dir,
                cryptsetup: Luks::new(program),
            }
        }

        fn exit_with(&self, call: usize, code: i32) {
            std::fs::write(self.dir.join(format!("code.{call}")), format!("{code}\n")).unwrap();
        }

        fn read(&self, what: &str, call: usize) -> Vec<u8> {
            std::fs::read(self.dir.join(format!("{what}.{call}"))).unwrap()
        }

        fn argv(&self, call: usize) -> Vec<String> {
            String::from_utf8(self.read("argv", call))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    #[test]
    fn a_child_gets_its_input_and_no_environment() {
        let recorder = Recorder::new();
        recorder
            .cryptsetup
            .run(&close_args("td-install-07"), b"input")
            .unwrap();
        assert_eq!(recorder.argv(1), ["close", "td-install-07"]);
        assert_eq!(recorder.read("stdin", 1), b"input");
        let environment = String::from_utf8(recorder.read("env", 1)).unwrap();
        assert!(!environment.contains("PATH"), "{environment}");
        assert!(!environment.contains("HOME"), "{environment}");
        recorder.exit_with(2, 1);
        let error = recorder
            .cryptsetup
            .run(&close_args("td-install-07"), b"")
            .unwrap_err();
        assert!(error.to_string().contains("close failed"), "{error}");
    }

    /// The second secret reaches the child only through the descriptor's
    /// name; neither secret is in its argv.
    #[test]
    fn a_new_key_travels_by_descriptor_and_no_key_in_argv() {
        let recorder = Recorder::new();
        let passphrase = [b'7'; 48];
        let secret = [0xa5; 32];
        let device = Path::new("/dev/loop7");
        let new_key = KeyFile::new(&secret).unwrap();
        recorder
            .cryptsetup
            .run(&add_key_args(device, 0, 1, new_key.path()), &passphrase)
            .unwrap();
        drop(new_key);
        assert_eq!(recorder.read("stdin", 1), passphrase);
        assert_eq!(recorder.read("key", 1), secret);
        for arg in recorder.argv(1) {
            for key in [&passphrase[..], &secret[..]] {
                assert!(!arg.as_bytes().windows(key.len()).any(|w| w == key));
            }
        }
    }

    /// An input shorter than td's shortest secret is refused
    /// before any child starts; a protector's or the recovery key's runs.
    #[test]
    fn a_kill_without_a_whole_key_never_starts() {
        let recorder = Recorder::new();
        let device = Path::new("/dev/loop7");
        for key in [&[][..], &[7; MIN_KILL_KEY - 1][..]] {
            let error = recorder
                .cryptsetup
                .run(&kill_slot_args(device, 1), key)
                .unwrap_err();
            assert!(error.to_string().contains("slot removal needs"), "{error}");
        }
        assert!(!recorder.dir.join("count").exists());
        recorder
            .cryptsetup
            .run(&kill_slot_args(device, 1), &[7; MIN_KILL_KEY])
            .unwrap();
        recorder
            .cryptsetup
            .run(&kill_slot_args(device, 1), &[b'1'; 48])
            .unwrap();
        assert_eq!(recorder.read("stdin", 2), [b'1'; 48]);
        assert_eq!(MIN_KILL_KEY, 32);
    }

    /// The metadata dump's output is returned, not echoed, and a failed
    /// dump is an error.
    #[test]
    fn the_metadata_dump_returns_its_output() {
        let recorder = Recorder::new();
        let device = Path::new("/dev/loop7");
        assert_eq!(recorder.cryptsetup.metadata(device).unwrap(), b"out-1\n");
        assert_eq!(recorder.argv(1), ["metadata", "/dev/loop7"]);
        recorder.exit_with(2, 1);
        let error = recorder.cryptsetup.metadata(device).unwrap_err();
        assert!(error.to_string().contains("metadata failed"), "{error}");
    }

    /// A program named by a relative path is refused, not looked up.
    #[test]
    fn a_relative_program_is_refused() {
        let error = Luks::new(PathBuf::from("cryptsetup"))
            .run(&close_args(NAME), b"")
            .unwrap_err();
        assert!(
            error.to_string().contains("not an absolute path"),
            "{error}"
        );
        assert_eq!(exit_code(&error), None);
    }

    /// A failed run carries its exit code, so a caller can tell a wrong
    /// passphrase (2) from every other failure; the message names the
    /// program, the verb and the status alone.
    #[test]
    fn a_failed_run_carries_its_exit_code() {
        let recorder = Recorder::new();
        let device = Path::new("/dev/loop7");
        for (call, code) in [(1, EXIT_BAD_PASSPHRASE), (2, 1), (3, 4)] {
            recorder.exit_with(call, code);
            let error = recorder
                .cryptsetup
                .run(&test_args(device, 0), b"secret")
                .unwrap_err();
            assert_eq!(exit_code(&error), Some(code), "{error}");
            assert!(error.to_string().contains(" test failed ("), "{error}");
            assert!(!error.to_string().contains("secret"), "{error}");
        }
        assert_eq!(EXIT_BAD_PASSPHRASE, 2);
        assert_eq!(exit_code(&io::Error::other("other")), None);
    }

    /// `status` is read by its exit alone: 0 active, 4 inactive, and any
    /// other exit, such as 1 for a device-mapper that cannot be asked, an
    /// error rather than either.
    #[test]
    fn a_mapping_is_classified_by_its_status_exit() {
        let recorder = Recorder::new();
        let cryptsetup = &recorder.cryptsetup;
        assert_eq!(cryptsetup.mapping(NAME).unwrap(), Mapping::Active);
        for (call, code) in [(2, 1), (3, 2), (4, 5)] {
            recorder.exit_with(call, code);
            let error = cryptsetup.mapping(NAME).unwrap_err();
            assert!(error.to_string().contains("status failed"), "{error}");
        }
        recorder.exit_with(5, STATUS_INACTIVE);
        assert_eq!(cryptsetup.mapping(NAME).unwrap(), Mapping::Inactive);
        assert_eq!(recorder.argv(5), ["status", NAME]);
        assert!(!recorder.dir.join(NAME).exists());
    }

    /// A bounded run passes the key by standard input alone, reports the
    /// exit, and kills and reaps a child that outlasts its deadline or
    /// whose caller is abandoned; the refusals every run has still apply.
    #[test]
    fn a_bounded_run_kills_and_reaps_a_stalled_child() {
        let recorder = Recorder::new();
        let device = Path::new("/dev/loop7");
        let far = Instant::now() + Duration::from_secs(60);
        let args = detached_test_args(Path::new("/run/h"), device, 1);
        match recorder
            .cryptsetup
            .run_bounded(&args, b"secret", far, &mut || false)
            .unwrap()
        {
            Bounded::Exited(status) => assert!(status.success()),
            other => panic!("{other:?}"),
        }
        assert_eq!(recorder.read("stdin", 1), b"secret");
        recorder.exit_with(2, EXIT_BAD_PASSPHRASE);
        match recorder
            .cryptsetup
            .run_bounded(&args, b"secret", far, &mut || false)
            .unwrap()
        {
            Bounded::Exited(status) => assert_eq!(status.code(), Some(EXIT_BAD_PASSPHRASE)),
            other => panic!("{other:?}"),
        }
        let sleep = tool("sleep");
        let stalled = recorder.dir.join("stalled");
        let body = format!(
            "#!/bin/sh\necho $$ > '{dir}/pid.'$1\nexec '{sleep}' 60\n",
            dir = recorder.dir.display(),
            sleep = sleep.display()
        );
        let written = Command::new("/bin/sh")
            .args([
                "-c",
                "printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"",
                "sh",
            ])
            .arg(&stalled)
            .arg(body)
            .status()
            .unwrap();
        assert!(written.success());
        let stalled = Luks::new(stalled);
        let gone = |name: &str| {
            let path = recorder.dir.join(name);
            let pid = (0..500)
                .find_map(|_| {
                    let pid = std::fs::read_to_string(&path).ok();
                    if pid.is_none() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    pid
                })
                .unwrap();
            !Path::new(&format!("/proc/{}", pid.trim())).exists()
        };
        let started = Instant::now();
        let ended = stalled
            .run_bounded(
                &os(&["expired"]),
                b"secret",
                Instant::now() + Duration::from_millis(300),
                &mut || false,
            )
            .unwrap();
        assert!(matches!(ended, Bounded::Expired), "{ended:?}");
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(gone("pid.expired"));
        let mut looks = 0;
        let ended = stalled
            .run_bounded(&os(&["abandoned"]), b"secret", far, &mut || {
                looks += 1;
                looks > 20
            })
            .unwrap();
        assert!(matches!(ended, Bounded::Abandoned), "{ended:?}");
        assert!(gone("pid.abandoned"));
        // The refusals `run` has hold before any child starts.
        assert!(Luks::new(PathBuf::from("cryptsetup"))
            .run_bounded(&args, b"secret", far, &mut || false)
            .is_err());
        assert!(recorder
            .cryptsetup
            .run_bounded(&kill_slot_args(device, 1), &[], far, &mut || false)
            .is_err());
        assert_eq!(std::fs::read(recorder.dir.join("count")).unwrap(), b"2\n");
    }
}
