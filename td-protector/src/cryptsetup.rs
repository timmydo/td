//! The cryptsetup runner the installer and the selector share (DESIGN.md
//! "Cryptsetup runner"): each command's exact arguments, and one child per
//! command with key material by descriptor only.
//!
//! Every child gets a cleared environment and an absolute program. The
//! first secret goes on standard input; the second, the new key `luksAddKey`
//! reads as its key file, through a std pipe this process keeps open and
//! names as `/proc/<pid>/fd/N`. No argv element, environment variable or file
//! carries key material, and nothing here is `unsafe`.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// The most a child's input may be: one page, which any pipe holds, so
/// filling it never blocks. A secret is at most 48 bytes, and a token's JSON
/// is held to `token::MAX_TOKEN_JSON`, no more than this.
pub const MAX_PIPE_INPUT: usize = 4096;

/// The shortest key `luksKillSlot` takes on standard input: td's shortest
/// secret, a protector's. cryptsetup 2.8.8 destroys the keyslot unasked
/// when that input is empty (`action_luksKillSlot` ignores `-EPIPE`), so
/// the runner refuses a shorter one before any child starts.
pub const MIN_KILL_KEY: usize = crate::SECRET_LEN;

/// The most standard output the runner reads from one child, the metadata
/// dump's bound; a child writing more fails.
pub const MAX_OUTPUT: usize = crate::luks2::MAX_METADATA_JSON;

/// cryptsetup 2.8.8's exit for an inactive `status`: `action_status` returns
/// `-ENODEV` for `CRYPT_INACTIVE`, which `translate_errno` makes 4. An
/// active mapping exits 0; anything else (`CRYPT_INVALID` exits 1) says
/// neither.
pub const STATUS_INACTIVE: i32 = 4;

/// `luksFormat`'s fixed arguments, in ENCRYPTION.md's order, up to the UUID.
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
];

/// The PBKDF every keyslot takes: `luksFormat`'s, for `luksAddKey`.
const PBKDF: &[&str] = &[
    "--pbkdf",
    "pbkdf2",
    "--pbkdf-force-iterations",
    "1000",
    "--hash",
    "sha256",
];

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// `luksFormat` of `device` as keyslot 0, its passphrase on standard input.
pub fn format_args(uuid: &str, label: &str, device: &Path) -> Vec<OsString> {
    let mut args = os(FORMAT);
    args.extend(os(&[
        "--uuid",
        uuid,
        "--label",
        label,
        "--key-slot",
        "0",
        "--key-file=-",
    ]));
    args.push(device.into());
    args
}

/// `luksAddKey` of keyslot `new_slot`, keyslot `by`'s key on standard input
/// authorizing it, the new key read from `new_key`.
pub fn add_key_args(device: &Path, by: u8, new_slot: u8, new_key: &Path) -> Vec<OsString> {
    let mut args = os(&["luksAddKey", "--batch-mode"]);
    args.extend(os(PBKDF));
    args.push("--key-slot".into());
    args.push(by.to_string().into());
    args.push("--new-key-slot".into());
    args.push(new_slot.to_string().into());
    args.push("--key-file=-".into());
    args.push(device.into());
    args.push(new_key.into());
    args
}

/// `token import` of the JSON on standard input, as token `id`, or as the
/// lowest free token number when `None`.
pub fn token_import_args(device: &Path, id: Option<u8>) -> Vec<OsString> {
    let mut args = os(&["token", "import"]);
    if let Some(id) = id {
        args.push("--token-id".into());
        args.push(id.to_string().into());
    }
    args.push("--json-file=-".into());
    args.push(device.into());
    args
}

/// `token remove` of token `id`.
pub fn token_remove_args(device: &Path, id: u8) -> Vec<OsString> {
    let mut args = os(&["token", "remove", "--token-id"]);
    args.push(id.to_string().into());
    args.push(device.into());
    args
}

/// `luksKillSlot` of `slot`, the key of another keyslot on standard input:
/// with a key, cryptsetup destroys the keyslot only once that key opens a
/// keyslot other than `slot`. Without one it would destroy it unasked, so
/// `Cryptsetup::run` refuses a key shorter than `MIN_KILL_KEY`.
pub fn kill_slot_args(device: &Path, slot: u8) -> Vec<OsString> {
    let mut args = os(&["luksKillSlot", "--batch-mode", "--key-file=-"]);
    args.push(device.into());
    args.push(slot.to_string().into());
    args
}

/// `luksDump --dump-json-metadata`: the JSON of the header copy
/// cryptsetup itself uses, on standard output. It holds no key material.
pub fn dump_metadata_args(device: &Path) -> Vec<OsString> {
    let mut args = os(&["luksDump", "--dump-json-metadata"]);
    args.push(device.into());
    args
}

/// `luksDump --dump-volume-key` of `device` into `file`, unlocked by the key
/// of a keyslot on standard input, for the selector's handoff. cryptsetup
/// 2.8.8 (`tools_write_mk`) creates `file` with `O_CREAT|O_EXCL` at mode
/// 0400, so a name that exists, a symlink or a `/proc/self/fd` link
/// included, is refused, and writes the raw key there; standard output then
/// carries the header's summary and the file's name, never the key.
/// `--batch-mode` answers its confirmation.
pub fn dump_volume_key_args(device: &Path, file: &Path) -> Vec<OsString> {
    let mut args = os(&[
        "luksDump",
        "--dump-volume-key",
        "--batch-mode",
        "--volume-key-file",
    ]);
    args.push(file.into());
    args.push("--key-file=-".into());
    args.push(device.into());
    args
}

/// Whether `args` is exactly the shape `dump_metadata_args` or
/// `dump_volume_key_args` builds, with operands that cannot be read as
/// options: the only luksDump forms the runner starts.
fn known_dump(args: &[OsString]) -> bool {
    let operand = |arg: &OsString| !arg.is_empty() && !arg.as_encoded_bytes().starts_with(b"-");
    match args {
        [verb, flag, device] => {
            verb == "luksDump" && flag == "--dump-json-metadata" && operand(device)
        }
        [verb, dump, batch, to, file, key, device] => {
            verb == "luksDump"
                && dump == "--dump-volume-key"
                && batch == "--batch-mode"
                && to == "--volume-key-file"
                && operand(file)
                && key == "--key-file=-"
                && operand(device)
        }
        _ => false,
    }
}

/// The mapping `name`, opened with the passphrase on standard input.
pub fn open_args(device: &Path, name: &str) -> Vec<OsString> {
    let mut args = os(&["open", "--type", "luks2", "--key-file=-"]);
    args.push(device.into());
    args.push(name.into());
    args
}

/// The mapping `name`, opened with the passphrase on standard input tried
/// on keyslot `slot` alone: the selector's recovery flow opens with the
/// recovery key on keyslot 0 this way.
pub fn open_slot_args(device: &Path, name: &str, slot: u8) -> Vec<OsString> {
    let mut args = os(&["open", "--type", "luks2", "--key-slot"]);
    args.push(slot.to_string().into());
    args.push("--key-file=-".into());
    args.push(device.into());
    args.push(name.into());
    args
}

/// The mapping `name`, opened with the raw volume key cryptsetup reads from
/// `key`, a `KeyFile`'s descriptor name. cryptsetup 2.8.8's `open` with
/// `--volume-key-file` reads the header's volume-key size from it
/// (`tools_read_vk`) and activates only a key that matches the header's
/// digest of the data segment (`_verify_key`,
/// `LUKS2_digest_verify_by_segment`); it tries no token or keyslot.
pub fn open_by_volume_key_args(device: &Path, name: &str, key: &Path) -> Vec<OsString> {
    let mut args = os(&["open", "--type", "luks2", "--volume-key-file"]);
    args.push(key.into());
    args.push(device.into());
    args.push(name.into());
    args
}

pub fn close_args(name: &str) -> Vec<OsString> {
    os(&["close", name])
}

/// Whether the mapping `name` is active, asked of device-mapper by name:
/// no node under `/dev/mapper` is consulted.
pub fn status_args(name: &str) -> Vec<OsString> {
    os(&["status", name])
}

/// Whether `slot` opens with the key on standard input; nothing is mapped.
pub fn test_args(device: &Path, slot: u8) -> Vec<OsString> {
    let mut args = os(&["open", "--test-passphrase", "--type", "luks2", "--key-slot"]);
    args.push(slot.to_string().into());
    args.push("--key-file=-".into());
    args.push(device.into());
    args
}

/// What `status` said of a mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mapping {
    Active,
    Inactive,
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

/// A static cryptsetup at an absolute path.
pub struct Cryptsetup {
    program: PathBuf,
}

impl Cryptsetup {
    /// The program is checked at each run: a path that is not absolute is
    /// refused rather than looked up.
    pub fn new(program: PathBuf) -> Self {
        Self { program }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Runs one cryptsetup command with `input` on its standard input. Its
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

    /// The JSON metadata of the header copy cryptsetup uses on `device`,
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
        if !self.program.is_absolute() {
            return Err(invalid(format!(
                "cannot run {}: not an absolute path",
                self.program.display()
            )));
        }
        // Other luksDump forms can print key material to the standard output
        // `run` copies to standard error: `--dump-volume-key` (or its alias
        // `--dump-master-key`) without a file, and `--unbound`. cryptsetup
        // takes options before its action too, so the verb is looked for
        // anywhere.
        if args.iter().any(|arg| arg == "luksDump") && !known_dump(args) {
            return Err(invalid(
                "luksDump runs only as --dump-json-metadata or as --dump-volume-key \
                 into a --volume-key-file"
                    .to_string(),
            ));
        }
        if args.first().is_some_and(|verb| verb == "luksKillSlot") && input.len() < MIN_KILL_KEY {
            return Err(invalid(format!(
                "luksKillSlot needs a key of at least {MIN_KILL_KEY} bytes on standard input, \
                 not {}",
                input.len()
            )));
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{Role, Token, MAX_PRIVATE_BYTES, MAX_PUBLIC_BYTES, MAX_TOKEN_JSON};
    use std::fs::File;
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use td_tpm::SealedObject;

    const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
    const NAME: &str = "td-install-0707070707070707";

    fn words(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    /// The arguments are ENCRYPTION.md's and DESIGN.md's, word for word.
    #[test]
    fn each_command_is_spelled_exactly() {
        let device = Path::new("/dev/loop7");
        assert_eq!(
            words(&format_args(UUID, "td-system", device)),
            [
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
                "/dev/loop7",
            ]
        );
        assert_eq!(
            words(&add_key_args(device, 0, 1, Path::new("/proc/9/fd/4"))),
            [
                "luksAddKey",
                "--batch-mode",
                "--pbkdf",
                "pbkdf2",
                "--pbkdf-force-iterations",
                "1000",
                "--hash",
                "sha256",
                "--key-slot",
                "0",
                "--new-key-slot",
                "1",
                "--key-file=-",
                "/dev/loop7",
                "/proc/9/fd/4",
            ]
        );
        assert_eq!(
            words(&add_key_args(device, 1, 12, Path::new("/proc/9/fd/4")))[8..13],
            ["--key-slot", "1", "--new-key-slot", "12", "--key-file=-"]
        );
        assert_eq!(
            words(&token_import_args(device, Some(0))),
            [
                "token",
                "import",
                "--token-id",
                "0",
                "--json-file=-",
                "/dev/loop7"
            ]
        );
        assert_eq!(
            words(&token_import_args(device, None)),
            ["token", "import", "--json-file=-", "/dev/loop7"]
        );
        assert_eq!(
            words(&token_remove_args(device, 3)),
            ["token", "remove", "--token-id", "3", "/dev/loop7"]
        );
        assert_eq!(
            words(&kill_slot_args(device, 2)),
            [
                "luksKillSlot",
                "--batch-mode",
                "--key-file=-",
                "/dev/loop7",
                "2"
            ]
        );
        assert_eq!(
            words(&open_args(device, "td-install-07")),
            [
                "open",
                "--type",
                "luks2",
                "--key-file=-",
                "/dev/loop7",
                "td-install-07"
            ]
        );
        assert_eq!(
            words(&open_slot_args(Path::new("/proc/1/fd/5"), "td-selector", 0)),
            [
                "open",
                "--type",
                "luks2",
                "--key-slot",
                "0",
                "--key-file=-",
                "/proc/1/fd/5",
                "td-selector"
            ]
        );
        assert_eq!(
            words(&open_by_volume_key_args(
                Path::new("/proc/9/fd/5"),
                "td-system",
                Path::new("/proc/9/fd/4")
            )),
            [
                "open",
                "--type",
                "luks2",
                "--volume-key-file",
                "/proc/9/fd/4",
                "/proc/9/fd/5",
                "td-system"
            ]
        );
        assert_eq!(
            words(&close_args("td-install-07")),
            ["close", "td-install-07"]
        );
        assert_eq!(
            words(&status_args("td-install-07")),
            ["status", "td-install-07"]
        );
        assert_eq!(
            words(&dump_metadata_args(device)),
            ["luksDump", "--dump-json-metadata", "/dev/loop7"]
        );
        assert_eq!(
            words(&dump_volume_key_args(
                Path::new("/proc/1/fd/5"),
                Path::new("/run/td-boot-volume-key/volume-key")
            )),
            [
                "luksDump",
                "--dump-volume-key",
                "--batch-mode",
                "--volume-key-file",
                "/run/td-boot-volume-key/volume-key",
                "--key-file=-",
                "/proc/1/fd/5",
            ]
        );
        for slot in [0, 1, 31] {
            let slot_text = slot.to_string();
            assert_eq!(
                words(&test_args(device, slot)),
                [
                    "open",
                    "--test-passphrase",
                    "--type",
                    "luks2",
                    "--key-slot",
                    slot_text.as_str(),
                    "--key-file=-",
                    "/dev/loop7",
                ]
            );
        }
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

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// A cryptsetup stand-in in a fresh directory: each call's argv,
    /// exported environment, standard input and any `/proc` key file it
    /// names, numbered in order. A `code.N` file makes call N exit with the
    /// code it holds.
    struct Recorder {
        dir: PathBuf,
        cryptsetup: Cryptsetup,
    }

    impl Recorder {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "td-protector-cryptsetup-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&dir).unwrap();
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
                cryptsetup: Cryptsetup::new(program),
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

    impl Drop for Recorder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
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

    /// cryptsetup destroys a keyslot unasked when `luksKillSlot`'s input is
    /// empty, so an input shorter than td's shortest secret is refused
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
            assert!(error.to_string().contains("luksKillSlot needs"), "{error}");
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

    /// A luksDump that could print key material to the output `run` copies
    /// to standard error never starts: a volume-key dump without a file,
    /// cryptsetup 2.8.8's `--dump-master-key` and `--master-key-file`
    /// aliases, `--unbound`, an operand read as an option, or options before
    /// the verb. Only the two shapes this module builds run.
    #[test]
    fn a_volume_key_dump_runs_only_into_a_file() {
        let recorder = Recorder::new();
        let device = Path::new("/dev/loop7");
        let refused: &[&[&str]] = &[
            &[
                "luksDump",
                "--dump-volume-key",
                "--batch-mode",
                "--key-file=-",
                "/dev/loop7",
            ],
            &[
                "luksDump",
                "--dump-master-key",
                "--batch-mode",
                "--key-file=-",
                "/dev/loop7",
            ],
            &[
                "luksDump",
                "--dump-master-key",
                "--batch-mode",
                "--master-key-file",
                "/run/k",
                "--key-file=-",
                "/dev/loop7",
            ],
            &[
                "luksDump",
                "--dump-volume-key",
                "--batch-mode",
                "--volume-key-file",
                "--unbound",
                "--key-file=-",
                "/dev/loop7",
            ],
            &[
                "luksDump",
                "--unbound",
                "--key-slot",
                "1",
                "--batch-mode",
                "--key-file=-",
                "/dev/loop7",
            ],
            &["luksDump", "--dump-json-metadata", "--unbound"],
            &["luksDump", "/dev/loop7"],
            &["luksDump"],
            &[
                "--dump-volume-key",
                "--batch-mode",
                "--key-file=-",
                "luksDump",
                "/dev/loop7",
            ],
            &["--unbound", "luksDump", "/dev/loop7"],
        ];
        for args in refused {
            let error = recorder.cryptsetup.run(&os(args), &[7; 32]).unwrap_err();
            assert!(error.to_string().contains("--volume-key-file"), "{error}");
        }
        assert!(!recorder.dir.join("count").exists());
        let file = recorder.dir.join("volume-key");
        recorder
            .cryptsetup
            .run(&dump_volume_key_args(device, &file), &[7; 32])
            .unwrap();
        assert_eq!(recorder.read("stdin", 1), [7; 32]);
    }

    /// The metadata dump's output is returned, not echoed, and a failed
    /// dump is an error.
    #[test]
    fn the_metadata_dump_returns_its_output() {
        let recorder = Recorder::new();
        let device = Path::new("/dev/loop7");
        assert_eq!(recorder.cryptsetup.metadata(device).unwrap(), b"out-1\n");
        assert_eq!(
            recorder.argv(1),
            ["luksDump", "--dump-json-metadata", "/dev/loop7"]
        );
        recorder.exit_with(2, 1);
        let error = recorder.cryptsetup.metadata(device).unwrap_err();
        assert!(error.to_string().contains("luksDump failed"), "{error}");
    }

    /// A program named by a relative path is refused, not looked up.
    #[test]
    fn a_relative_program_is_refused() {
        let error = Cryptsetup::new(PathBuf::from("cryptsetup"))
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
            assert!(error.to_string().contains(" open failed ("), "{error}");
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
}
