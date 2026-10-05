//! Device-bound formatting on a loop over the claimed volume (DESIGN.md
//! "Device-bound formatting", ENCRYPTION.md "Device-bound formatting"):
//! the order of cryptsetup's commands, the first-boot protector and the
//! checks verifying boot adds. td-protector's runner spells each command
//! and passes key material by descriptor only (td-protector/DESIGN.md
//! "Cryptsetup runner").

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;

use td_protector::cryptsetup::{
    add_key_args, close_args, format_args, open_args, test_args, token_import_args, Cryptsetup,
    KeyFile, Mapping,
};
use td_protector::luks2;
use td_protector::recovery::RecoveryKey;
use td_protector::token::{Role, Token, MAX_TOKEN_JSON};
use td_protector::Secret;
use td_tpm::SealedObject;

use crate::{invalid, protocol, FormatStep};

/// Both LUKS2 header copies and the keyslots area: the data segment starts
/// here, and the volume fit and minimum volume size subtract it.
pub(crate) const HEADER_BYTES: u64 = 16 * 1024 * 1024;
/// The data segment's encryption sector. The loop over the volume is this
/// many bytes a block whole, since cryptsetup refuses a device that is not.
pub(crate) const SECTOR_BYTES: u64 = 4096;
const RECOVERY_SLOT: u8 = 0;
const PROTECTOR_SLOT: u8 = 1;
/// The first-boot protector's token number.
const PROTECTOR_TOKEN: u8 = 0;
/// How many times a mapping is asked to close, `CLOSE_RETRY` apart, before
/// it is reported as surviving.
const CLOSE_ATTEMPTS: usize = 3;
#[cfg(not(test))]
const CLOSE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(test)]
const CLOSE_RETRY: std::time::Duration = std::time::Duration::from_millis(1);

/// `luksFormat` with the plan's UUID and td's volume label.
fn format_volume_args(uuid: &str, device: &Path) -> Vec<OsString> {
    format_args(uuid, protocol::VOLUME_LABEL, device)
}

/// `luksAddKey` of keyslot 1, keyslot 0's recovery passphrase authorizing it.
fn add_protector_args(device: &Path, new_key: &Path) -> Vec<OsString> {
    add_key_args(device, RECOVERY_SLOT, PROTECTOR_SLOT, new_key)
}

/// Token 0, its JSON on standard input.
fn import_protector_args(device: &Path) -> Vec<OsString> {
    token_import_args(device, Some(PROTECTOR_TOKEN))
}

/// Seals and checks the first-boot protector. Production's is the live TPM,
/// a fresh client per operation.
pub(crate) trait Protector {
    fn seal(&self, secret: &Secret) -> Result<SealedObject, String>;
    fn verify(&self, sealed: &SealedObject) -> Result<(), String>;
}

/// `/dev/tpmrm0`, opened afresh for each operation.
pub(crate) struct Tpm;

impl Protector for Tpm {
    fn seal(&self, secret: &Secret) -> Result<SealedObject, String> {
        let client = td_tpm::Client::new(td_tpm::Device::open()?);
        td_protector::seal(client, &td_protector::first_boot_policy()?, secret)
    }

    fn verify(&self, sealed: &SealedObject) -> Result<(), String> {
        let mut client = td_tpm::Client::new(td_tpm::Device::open()?);
        td_protector::verify_first_boot_object(&mut client, sealed)
    }
}

/// A read-only window onto `len` bytes of `file` from `offset`, read by
/// position: the volume as the claim holds it, for td-protector's reader.
pub(crate) struct Window<'a> {
    file: &'a File,
    offset: u64,
    len: u64,
    at: u64,
}

impl<'a> Window<'a> {
    pub(crate) fn new(file: &'a File, offset: u64, len: u64) -> Self {
        Self {
            file,
            offset,
            len,
            at: 0,
        }
    }
}

impl Read for Window<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let left = self.len.saturating_sub(self.at);
        let want = usize::try_from(left).unwrap_or(usize::MAX).min(out.len());
        let Some(buffer) = out.get_mut(..want) else {
            return Ok(0);
        };
        if buffer.is_empty() {
            return Ok(0);
        }
        let at = self
            .offset
            .checked_add(self.at)
            .ok_or_else(|| invalid("volume read offset overflowed".into()))?;
        let read = self.file.read_at(buffer, at)?;
        self.at = self.at.saturating_add(read as u64);
        Ok(read)
    }
}

impl Seek for Window<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let at = match to {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.at.checked_add_signed(delta),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek outside the volume"))?;
        self.at = at;
        Ok(at)
    }
}

/// The header as formatted: read from a valid primary copy no older than
/// the secondary, with the plan's UUID and the label, keyslots 0 and 1
/// alone, exactly the token imported as token 0, and no orphan. The reader
/// says neither whether both copies were valid nor what the data segment
/// is: the keyslot tests cover the segment, opening it.
pub(crate) fn check_header(
    header: &luks2::Header,
    uuid: &str,
    token: &Token,
) -> Result<(), String> {
    if header.copy != luks2::HeaderCopy::Primary {
        return Err("the LUKS2 header was read from its secondary copy".into());
    }
    if header.uuid != uuid {
        return Err(format!(
            "the LUKS2 header names UUID {:?}, not {uuid}",
            header.uuid
        ));
    }
    if header.label != protocol::VOLUME_LABEL {
        return Err(format!(
            "the LUKS2 header is labelled {:?}, not {}",
            header.label,
            protocol::VOLUME_LABEL
        ));
    }
    if header.keyslots != [RECOVERY_SLOT, PROTECTOR_SLOT] {
        return Err(format!(
            "the LUKS2 header holds keyslots {:?}, not 0 and 1",
            header.keyslots
        ));
    }
    if header.tokens.len() != 1
        || header.tokens.first() != Some(&(PROTECTOR_TOKEN, token.clone()))
        || !header.orphans.is_empty()
    {
        return Err("the LUKS2 header does not carry exactly the token imported".into());
    }
    Ok(())
}

/// Where a device-bound format stopped once the disk was written: writing,
/// or the checks verifying boot adds while the loop is bound.
#[derive(Debug)]
pub(crate) enum Stopped {
    Write(io::Error),
    Verify(io::Error),
}

/// What the volume format runs besides cryptsetup, so tests can stand in.
pub(crate) struct Volume<'a> {
    pub(crate) cryptsetup: &'a Cryptsetup,
    pub(crate) protector: &'a dyn Protector,
    /// The plan's volume UUID, canonical.
    pub(crate) uuid: &'a str,
    /// The mapping's name, and the directory its node appears in.
    pub(crate) name: &'a str,
    pub(crate) mapper: &'a Path,
    /// `mkfs.btrfs` over the mapping with the data segment's length.
    pub(crate) mkfs: &'a dyn Fn(&Path, u64) -> io::Result<()>,
    /// `td-boot install` on the mapping; its standard output.
    pub(crate) publish: &'a dyn Fn(&Path) -> io::Result<Vec<u8>>,
}

/// Closes the mapping `name` until `status` says it is inactive: asked
/// first, so an open that failed before loading its table needs no close,
/// and after each close, so only device-mapper's own answer counts as
/// closed. A close is asked up to `CLOSE_ATTEMPTS` times; a status that is
/// neither active nor inactive is treated as active. A mapping still not
/// inactive at the end is reported as surviving: its volume key is still
/// live in the kernel.
fn close(cryptsetup: &Cryptsetup, name: &str, mapping: &Path) -> io::Result<()> {
    let mut failed = String::new();
    for attempt in 0..=CLOSE_ATTEMPTS {
        match cryptsetup.mapping(name) {
            Ok(Mapping::Inactive) => return Ok(()),
            Ok(Mapping::Active) => {}
            Err(error) => failed = format!(": {error}"),
        }
        if attempt == CLOSE_ATTEMPTS {
            break;
        }
        if let Err(error) = cryptsetup.run(&close_args(name), &[]) {
            failed = format!(": {error}");
            std::thread::sleep(CLOSE_RETRY);
        }
    }
    let cause = failed;
    Err(invalid(format!(
        "the mapping {} survives {CLOSE_ATTEMPTS} attempts to close it, its volume key \
         still live{cause}",
        mapping.display()
    )))
}

/// Formats `device`, a loop over `len` bytes of the claim from `offset`, and
/// publishes into it; then, the loop still bound, checks that each keyslot
/// opens with its key and that the header read back through `claim` carries
/// the token imported, which the TPM accepts. Returns td-boot's output and
/// the recovery key; the protector secret is zeroed on return.
pub(crate) fn format(
    volume: &Volume<'_>,
    device: &Path,
    len: u64,
    claim: &File,
    offset: u64,
    step: &mut dyn FnMut(FormatStep),
) -> Result<(Vec<u8>, RecoveryKey), Stopped> {
    let data = len
        .checked_sub(HEADER_BYTES)
        .filter(|data| *data > 0 && data % SECTOR_BYTES == 0)
        .ok_or_else(|| {
            Stopped::Write(invalid(format!(
                "a {len}-byte volume leaves no whole data segment after its LUKS2 header"
            )))
        })?;
    let write = Stopped::Write;
    let cryptsetup = volume.cryptsetup;
    let recovery = RecoveryKey::generate().map_err(|error| write(invalid(error)))?;
    let secret = Secret::generate().map_err(|error| write(invalid(error)))?;
    let passphrase = recovery.passphrase();
    cryptsetup
        .run(
            &format_volume_args(volume.uuid, device),
            passphrase.expose(),
        )
        .map_err(write)?;
    {
        let new_key = KeyFile::new(secret.expose()).map_err(write)?;
        cryptsetup
            .run(
                &add_protector_args(device, new_key.path()),
                passphrase.expose(),
            )
            .map_err(write)?;
    }
    let sealed = volume
        .protector
        .seal(&secret)
        .map_err(|error| write(invalid(format!("seal the first-boot protector: {error}"))))?;
    let token = Token::new(PROTECTOR_SLOT, Role::FirstBoot, sealed)
        .map_err(|error| write(invalid(error)))?;
    let encoded = token.encode();
    // What td-protector will read back, which also fits the runner's pipe.
    if encoded.len() > MAX_TOKEN_JSON {
        return Err(write(invalid(format!(
            "the first-boot token's {} bytes are over {MAX_TOKEN_JSON}",
            encoded.len()
        ))));
    }
    cryptsetup
        .run(&import_protector_args(device), encoded.as_bytes())
        .map_err(write)?;
    let mapping = volume.mapper.join(volume.name);
    // An open that failed may still have loaded its table: `close` asks.
    let published = cryptsetup
        .run(&open_args(device, volume.name), passphrase.expose())
        .and_then(|()| (volume.mkfs)(&mapping, data))
        .and_then(|()| {
            step(FormatStep::Publishing);
            (volume.publish)(&mapping)
        });
    // Closed whatever happened, so the loop can clear and no volume key
    // stays live in a mapping.
    let closed = close(cryptsetup, volume.name, &mapping);
    let stdout = match (published, closed) {
        (Ok(stdout), Ok(())) => stdout,
        (Err(error), Ok(())) => return Err(write(error)),
        (Ok(_), Err(error)) => return Err(write(error)),
        (Err(error), Err(close)) => {
            return Err(write(io::Error::new(
                error.kind(),
                format!("{error}; and {close}"),
            )))
        }
    };
    step(FormatStep::Verifying);
    let verify = |error: String| Stopped::Verify(invalid(error));
    for (slot, key) in [
        (RECOVERY_SLOT, passphrase.expose()),
        (PROTECTOR_SLOT, secret.expose().as_slice()),
    ] {
        cryptsetup
            .run(&test_args(device, slot), key)
            .map_err(|error| verify(format!("keyslot {slot} does not open: {error}")))?;
    }
    let header = luks2::read(&mut Window::new(claim, offset, len))
        .map_err(|error| verify(format!("read the LUKS2 header back: {error}")))?;
    check_header(&header, volume.uuid, &token).map_err(verify)?;
    volume
        .protector
        .verify(token.sealed())
        .map_err(|error| verify(format!("the TPM refuses the first-boot protector: {error}")))?;
    drop(passphrase);
    drop(secret);
    Ok((stdout, recovery))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use td_protector::cryptsetup::status_args;

    const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
    const NAME: &str = "td-install-0707070707070707";

    fn words(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    /// The arguments are ENCRYPTION.md's: the plan's UUID, td's label and
    /// keyslot 0 for the format, keyslot 1 authorized by keyslot 0, token
    /// 0. The runner pins the rest word for word, and the layout the format
    /// spells is the 16 MiB the volume fit subtracts.
    #[test]
    fn each_command_names_the_installation() {
        let device = Path::new("/dev/loop7");
        let format = format_volume_args(UUID, device);
        assert_eq!(
            words(&format)[23..],
            [
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
            words(&add_protector_args(device, Path::new("/proc/9/fd/4")))[8..],
            [
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
            words(&import_protector_args(device)),
            [
                "token",
                "import",
                "--token-id",
                "0",
                "--json-file=-",
                "/dev/loop7"
            ]
        );
        // Two 16 KiB header copies and the keyslots area fill the 16 MiB
        // before the data segment, whose offset counts 512-byte sectors.
        let at = |word: &str| {
            let index = words(&format).iter().position(|w| *w == word).unwrap();
            words(&format)[index + 1].parse::<u64>().unwrap()
        };
        assert_eq!(
            2 * at("--luks2-metadata-size") + at("--luks2-keyslots-size"),
            HEADER_BYTES
        );
        assert_eq!(at("--offset") * 512, HEADER_BYTES);
        assert_eq!(at("--sector-size"), SECTOR_BYTES);
        assert_eq!(protocol::VOLUME_LABEL, "td-system");
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

    /// A cryptsetup stand-in: each call's argv, exported environment,
    /// standard input and any `/proc` key file it names, numbered in order.
    /// A `fail.N` file makes call N exit with the code it holds, 1 if none.
    /// Otherwise it keeps one mapping's state as device-mapper would: open
    /// activates it, close deactivates it, and status exits 0 while it is
    /// active and 4 while not.
    struct Recorder {
        dir: PathBuf,
        cryptsetup: Cryptsetup,
    }

    impl Recorder {
        fn new() -> Self {
            let dir = scratch::path("cryptsetup");
            std::fs::create_dir(&dir).unwrap();
            let cat = tool("cat");
            let program = dir.join("cryptsetup");
            scratch::executable(
                &program,
                &format!(
                    "#!/bin/sh\nd='{dir}'\nn=0\nif [ -f \"$d/count\" ]; then read n < \"$d/count\"; fi\n\
                     n=$((n+1))\necho \"$n\" > \"$d/count\"\nprintf '%s\\n' \"$@\" > \"$d/argv.$n\"\n\
                     export -p > \"$d/env.$n\"\n'{cat}' > \"$d/stdin.$n\"\nfor last; do :; done\n\
                     case \"$last\" in /proc/*) '{cat}' \"$last\" > \"$d/key.$n\";; esac\n\
                     if [ -f \"$d/fail.$n\" ]; then c=1; read c < \"$d/fail.$n\" || :; exit \"${{c:-1}}\"; fi\n\
                     m=0\nif [ -f \"$d/mapped\" ]; then read m < \"$d/mapped\"; fi\n\
                     case \"$1 $2\" in\n\
                     'open --test-passphrase') ;;\n\
                     'open '*) echo 1 > \"$d/mapped\";;\n\
                     'close '*) echo 0 > \"$d/mapped\";;\n\
                     'status '*) if [ \"$m\" = 1 ]; then exit 0; fi; exit 4;;\n\
                     esac\n",
                    dir = dir.display(),
                    cat = cat.display()
                ),
            )
            .unwrap();
            Self {
                dir,
                cryptsetup: Cryptsetup::new(program),
            }
        }

        /// A mapping already active, as an open that failed after loading
        /// its table leaves one.
        fn mapped(&self) {
            std::fs::write(self.dir.join("mapped"), b"1\n").unwrap();
        }

        fn exit_with(&self, call: usize, code: i32) {
            std::fs::write(self.dir.join(format!("fail.{call}")), format!("{code}\n")).unwrap();
        }

        fn calls(&self) -> usize {
            std::fs::read_to_string(self.dir.join("count"))
                .map(|count| count.trim().parse().unwrap())
                .unwrap_or(0)
        }

        fn argv(&self, call: usize) -> Vec<String> {
            std::fs::read_to_string(self.dir.join(format!("argv.{call}")))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn read(&self, what: &str, call: usize) -> Vec<u8> {
            std::fs::read(self.dir.join(format!("{what}.{call}"))).unwrap()
        }

        fn fail(&self, call: usize) {
            std::fs::write(self.dir.join(format!("fail.{call}")), b"").unwrap();
        }
    }

    /// A fixed sealed object, and what each call was asked.
    #[derive(Default)]
    struct FakeTpm {
        sealed_secret: RefCell<Option<Vec<u8>>>,
        verified: RefCell<Vec<SealedObject>>,
        refuse: bool,
    }

    fn sealed() -> SealedObject {
        SealedObject {
            public: vec![0x00, 0x08, 0x01],
            private: vec![0x01, 0x02],
        }
    }

    impl Protector for FakeTpm {
        fn seal(&self, secret: &Secret) -> Result<SealedObject, String> {
            *self.sealed_secret.borrow_mut() = Some(secret.expose().to_vec());
            Ok(sealed())
        }
        fn verify(&self, sealed: &SealedObject) -> Result<(), String> {
            self.verified.borrow_mut().push(sealed.clone());
            if self.refuse {
                Err("TPM_RC_INTEGRITY".into())
            } else {
                Ok(())
            }
        }
    }

    /// One LUKS2 header copy as cryptsetup lays it out, checksummed here.
    fn copy(secondary: bool, json: &[u8], uuid: &str) -> Vec<u8> {
        let mut area = vec![0u8; 0x4000];
        area[..6].copy_from_slice(if secondary {
            b"SKUL\xba\xbe"
        } else {
            b"LUKS\xba\xbe"
        });
        area[6..8].copy_from_slice(&2u16.to_be_bytes());
        area[8..16].copy_from_slice(&0x4000u64.to_be_bytes());
        area[16..24].copy_from_slice(&3u64.to_be_bytes());
        area[24..33].copy_from_slice(b"td-system");
        area[72..78].copy_from_slice(b"sha256");
        area[104..168].fill(if secondary { 0x52 } else { 0x51 });
        area[168..168 + uuid.len()].copy_from_slice(uuid.as_bytes());
        let offset: u64 = if secondary { 0x4000 } else { 0 };
        area[256..264].copy_from_slice(&offset.to_be_bytes());
        area[4096..4096 + json.len()].copy_from_slice(json);
        let mut hasher = crate::sha256::Sha256::new();
        hasher.update(&area);
        let digest = hasher.finalize();
        area[448..480].copy_from_slice(&digest);
        area
    }

    fn header(keyslots: &str, tokens: &str, uuid: &str) -> Vec<u8> {
        let json = format!(
            "{{\"keyslots\":{{{keyslots}}},\"tokens\":{{{tokens}}},\"segments\":{{\"0\":{{\"type\":\"crypt\"}}}},\"digests\":{{}},\"config\":{{\"json_size\":\"12288\",\"keyslots_size\":\"16744448\"}}}}"
        );
        let mut out = copy(false, json.as_bytes(), uuid);
        out.extend(copy(true, json.as_bytes(), uuid));
        out
    }

    fn formatted_token() -> String {
        Token::new(1, Role::FirstBoot, sealed()).unwrap().encode()
    }

    const SLOTS: &str = "\"0\":{\"type\":\"luks2\"},\"1\":{\"type\":\"luks2\"}";

    /// A claim holding `header` one MiB in, and the volume's length.
    fn claim(header: &[u8]) -> (PathBuf, File, u64) {
        let path = scratch::path("claim");
        let len = HEADER_BYTES + 4 * SECTOR_BYTES;
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.set_len((1 << 20) + len).unwrap();
        file.write_all_at(header, 1 << 20).unwrap();
        (path, file, len)
    }

    struct Run {
        result: Result<(Vec<u8>, RecoveryKey), Stopped>,
        mkfs: Vec<(PathBuf, u64)>,
        published: Vec<PathBuf>,
        steps: Vec<FormatStep>,
    }

    fn run_format(recorder: &Recorder, tpm: &FakeTpm, header: &[u8], mkfs_fails: bool) -> Run {
        let (_path, file, len) = claim(header);
        let mkfs_calls = RefCell::new(Vec::new());
        let published = RefCell::new(Vec::new());
        let mkfs = |mapping: &Path, len: u64| {
            mkfs_calls.borrow_mut().push((mapping.to_path_buf(), len));
            if mkfs_fails {
                Err(invalid("mkfs.btrfs failed".into()))
            } else {
                Ok(())
            }
        };
        let publish = |mapping: &Path| {
            published.borrow_mut().push(mapping.to_path_buf());
            Ok(format!("{}\n", "ab".repeat(32)).into_bytes())
        };
        let volume = Volume {
            cryptsetup: &recorder.cryptsetup,
            protector: tpm,
            uuid: UUID,
            name: NAME,
            mapper: &recorder.dir,
            mkfs: &mkfs,
            publish: &publish,
        };
        let mut steps = Vec::new();
        let result = format(
            &volume,
            Path::new("/dev/loop7"),
            len,
            &file,
            1 << 20,
            &mut |step| steps.push(step),
        );
        Run {
            result,
            mkfs: mkfs_calls.into_inner(),
            published: published.into_inner(),
            steps,
        }
    }

    /// The whole sequence, in DESIGN.md's order: format, add keyslot 1,
    /// import the token, open, mkfs and publish on the mapping, close, then
    /// test each keyslot and read the header back. Key material travels on
    /// standard input or the `/proc` key file and never in argv: neither the
    /// passphrase nor the 32-byte protector secret appears in any argument.
    #[test]
    fn a_device_bound_volume_is_formatted_in_order_with_keys_by_descriptor() {
        let recorder = Recorder::new();
        let tpm = FakeTpm::default();
        let header = header(SLOTS, &format!("\"0\":{}", formatted_token()), UUID);
        let run = run_format(&recorder, &tpm, &header, false);
        let Ok((stdout, recovery)) = run.result else {
            panic!("{:?}", run.result.err())
        };
        assert_eq!(stdout, format!("{}\n", "ab".repeat(32)).into_bytes());
        let passphrase = recovery.passphrase().expose().to_vec();
        let secret = tpm.sealed_secret.borrow().clone().unwrap();
        assert_eq!(passphrase.len(), 48);
        assert_eq!(secret.len(), 32);
        let mapping = recorder.dir.join(NAME);
        assert_eq!(run.mkfs, [(mapping.clone(), 4 * SECTOR_BYTES)]);
        assert_eq!(run.published, [mapping]);
        assert_eq!(run.steps, [FormatStep::Publishing, FormatStep::Verifying]);
        assert_eq!(*tpm.verified.borrow(), [sealed()]);
        assert_eq!(recorder.calls(), 9);
        let device = Path::new("/dev/loop7");
        let key_path = recorder.argv(2).last().unwrap().clone();
        assert!(key_path.starts_with(&format!("/proc/{}/fd/", std::process::id())));
        let token = formatted_token();
        let expected: [(Vec<OsString>, &[u8]); 9] = [
            (format_volume_args(UUID, device), &passphrase),
            (
                add_protector_args(device, Path::new(&key_path)),
                &passphrase,
            ),
            (import_protector_args(device), token.as_bytes()),
            (open_args(device, NAME), &passphrase),
            (status_args(NAME), b""),
            (close_args(NAME), b""),
            (status_args(NAME), b""),
            (test_args(device, 0), &passphrase),
            (test_args(device, 1), &secret),
        ];
        for (call, (args, input)) in expected.iter().enumerate() {
            let call = call + 1;
            assert_eq!(recorder.argv(call), words(args), "call {call}");
            assert_eq!(recorder.read("stdin", call), *input, "call {call}");
            for arg in recorder.argv(call) {
                for key in [&passphrase, &secret] {
                    assert!(
                        !arg.as_bytes()
                            .windows(key.len())
                            .any(|window| window == key.as_slice()),
                        "a key in argv, call {call}"
                    );
                }
            }
        }
        // The new key, read by the child through the descriptor's name.
        assert_eq!(recorder.read("key", 2), secret);
    }

    #[test]
    fn a_failed_command_stops_the_sequence_as_a_write() {
        // luksFormat refused: nothing after it runs.
        let recorder = Recorder::new();
        recorder.fail(1);
        let tpm = FakeTpm::default();
        let run = run_format(&recorder, &tpm, &[], false);
        assert!(matches!(run.result, Err(Stopped::Write(_))));
        assert_eq!(recorder.calls(), 1);
        assert!(tpm.sealed_secret.borrow().is_none());
        // mkfs.btrfs failed: the mapping is still closed, nothing published.
        let recorder = Recorder::new();
        let run = run_format(&recorder, &FakeTpm::default(), &[], true);
        assert!(matches!(run.result, Err(Stopped::Write(_))));
        assert!(run.published.is_empty());
        assert_eq!(recorder.calls(), 7);
        assert_eq!(recorder.argv(6), ["close", NAME]);
        assert_eq!(recorder.argv(7), ["status", NAME]);
    }

    /// The mapping is closed until status says inactive, whatever
    /// happened: an open that failed is asked about, and closed if it left
    /// a mapping; a close that fails is asked again; a status that says
    /// neither counts as active; and a mapping still active after every
    /// close is reported as surviving.
    #[test]
    fn the_mapping_is_closed_and_a_survivor_reported() {
        let good = format!("\"0\":{}", formatted_token());
        let header = header(SLOTS, &good, UUID);
        let write_error = |run: Run| {
            let Err(Stopped::Write(error)) = run.result else {
                panic!("{:?}", run.result.err())
            };
            error.to_string()
        };
        // The open failed before mapping anything: status says inactive,
        // nothing is closed, and only the open is reported.
        let recorder = Recorder::new();
        recorder.fail(4);
        let run = run_format(&recorder, &FakeTpm::default(), &header, false);
        assert!(run.mkfs.is_empty() && run.published.is_empty());
        let error = write_error(run);
        assert!(
            error.contains("open failed") && !error.contains("survives"),
            "{error}"
        );
        assert_eq!(recorder.calls(), 5);
        assert_eq!(recorder.argv(5), ["status", NAME]);
        // The open failed after loading its table: the mapping is closed.
        let recorder = Recorder::new();
        recorder.mapped();
        recorder.fail(4);
        let error = write_error(run_format(&recorder, &FakeTpm::default(), &header, false));
        assert!(!error.contains("survives"), "{error}");
        assert_eq!(recorder.argv(6), ["close", NAME]);
        assert_eq!(recorder.argv(7), ["status", NAME]);
        assert_eq!(recorder.calls(), 7);
        // One failed close is asked again, and the format goes on.
        let recorder = Recorder::new();
        recorder.fail(6);
        let run = run_format(&recorder, &FakeTpm::default(), &header, false);
        assert!(run.result.is_ok(), "{:?}", run.result.err());
        assert_eq!(recorder.argv(8), ["close", NAME]);
        assert_eq!(recorder.argv(9), ["status", NAME]);
        assert_eq!(recorder.calls(), 11);
        // A status that says neither is no proof of a close: close is asked.
        let recorder = Recorder::new();
        recorder.exit_with(5, 1);
        let run = run_format(&recorder, &FakeTpm::default(), &header, false);
        assert!(run.result.is_ok(), "{:?}", run.result.err());
        assert_eq!(recorder.argv(6), ["close", NAME]);
        // Every close fails while status says active: a surviving mapping,
        // and nothing verified.
        let recorder = Recorder::new();
        for close in 0..CLOSE_ATTEMPTS {
            recorder.fail(6 + 2 * close);
        }
        let run = run_format(&recorder, &FakeTpm::default(), &header, false);
        assert!(run.steps.ends_with(&[FormatStep::Publishing]));
        let error = write_error(run);
        assert!(error.contains("survives"), "{error}");
        assert_eq!(recorder.calls(), 5 + 2 * CLOSE_ATTEMPTS);
        assert_eq!(recorder.argv(5 + 2 * CLOSE_ATTEMPTS), ["status", NAME]);
    }

    /// Each check verifying boot adds refuses as verification.
    #[test]
    fn verification_needs_both_keyslots_and_exactly_the_imported_token() {
        let good = format!("\"0\":{}", formatted_token());
        let other = Token::new(
            1,
            Role::FirstBoot,
            SealedObject {
                public: vec![0x00, 0x08, 0x02],
                private: vec![0x01, 0x02],
            },
        )
        .unwrap()
        .encode();
        let cases: Vec<(Option<usize>, Vec<u8>, bool)> = vec![
            // Keyslot 0, then keyslot 1, refuses its key.
            (Some(8), header(SLOTS, &good, UUID), false),
            (Some(9), header(SLOTS, &good, UUID), false),
            // No header, another UUID, a third keyslot, another token, a
            // second token, an orphan.
            (None, Vec::new(), false),
            (
                None,
                header(SLOTS, &good, "0f0e0d0c-0b0a-4908-8706-050403020100"),
                false,
            ),
            (
                None,
                header(
                    "\"0\":{\"type\":\"luks2\"},\"1\":{\"type\":\"luks2\"},\"2\":{\"type\":\"luks2\"}",
                    &good,
                    UUID,
                ),
                false,
            ),
            (None, header(SLOTS, &format!("\"0\":{other}"), UUID), false),
            (
                None,
                header(SLOTS, &format!("{good},\"1\":{other}"), UUID),
                false,
            ),
            (
                None,
                header(
                    SLOTS,
                    &format!(
                        "{good},\"1\":{{\"type\":\"td-protector\",\"keyslots\":[],\"role\":\"first-boot\",\"public\":\"00\",\"private\":\"00\"}}"
                    ),
                    UUID,
                ),
                false,
            ),
            // The TPM will not load the sealed object.
            (None, header(SLOTS, &good, UUID), true),
            // A primary copy that fails its checksum: read from the secondary.
            (
                None,
                {
                    let mut header = header(SLOTS, &good, UUID);
                    header[4096] ^= 1;
                    header
                },
                false,
            ),
        ];
        for (index, (fail, header, refuse)) in cases.into_iter().enumerate() {
            let recorder = Recorder::new();
            if let Some(call) = fail {
                recorder.fail(call);
            }
            let tpm = FakeTpm {
                refuse,
                ..FakeTpm::default()
            };
            let run = run_format(&recorder, &tpm, &header, false);
            assert!(
                matches!(run.result, Err(Stopped::Verify(_))),
                "case {index}: {:?}",
                run.result.as_ref().err()
            );
        }
    }

    #[test]
    fn a_window_reads_and_seeks_only_its_range() {
        let (_path, file, _) = claim(b"abc");
        let mut window = Window::new(&file, (1 << 20) + 1, 4);
        let mut read = Vec::new();
        window.read_to_end(&mut read).unwrap();
        assert_eq!(read, b"bc\0\0");
        assert_eq!(window.seek(SeekFrom::Start(1)).unwrap(), 1);
        let mut one = [0; 1];
        window.read_exact(&mut one).unwrap();
        assert_eq!(one, *b"c");
        assert_eq!(window.seek(SeekFrom::End(-1)).unwrap(), 3);
        assert!(window.seek(SeekFrom::Current(-9)).is_err());
        assert_eq!(window.seek(SeekFrom::End(5)).unwrap(), 9);
        assert_eq!(window.read(&mut one).unwrap(), 0);
    }
}
