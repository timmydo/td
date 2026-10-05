//! The deployment initramfs's unlock of a td LUKS2 volume with the volume
//! key the selector handed off (td-install/ENCRYPTION.md "Boot and authority
//! boundaries"): take the key member from the RAM-backed root, check that
//! the selector's cap closed TPM release, open the volume by descriptor
//! through td-protector's cryptsetup runner, and admit the mapping volume
//! discovery finds.
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use td_protector::cryptsetup::{self, Cryptsetup, KeyFile};
use td_protector::luks2::{self, Header};
use td_protector::release::{self, Tpm};
use td_protector::token::Token;
use td_protector::{Secret, UnsealError};

use crate::{invalid, protocol, volume};

/// x86-64 Linux O_NOFOLLOW | O_NONBLOCK: a symlink or a FIFO swapped in
/// after the check neither redirects nor blocks the open.
const OPEN_FLAGS: i32 = 0o400000 | 0o4000;
/// The member's mode, as td-kexec writes it.
const KEY_MODE: u32 = 0o400;
/// Where the kernel's `CONFIG_BLK_DEV_RAM` fallback writes an initrd it
/// could not unpack, the handoff archive and its key included.
pub(crate) const INITRD_IMAGE: &str = "initrd.image";
/// The TPM resource manager td-tpm opens.
pub(crate) const TPM_DEVICE: &str = "/dev/tpmrm0";

/// The handed-off volume key, in one heap allocation zeroed on drop: the
/// selector's for the handoff, the deployment initramfs's as taken. It is
/// neither `Debug`, `Display` nor `Clone`.
pub(crate) struct VolumeKey(Box<[u8; protocol::VOLUME_KEY_BYTES]>);

impl Drop for VolumeKey {
    fn drop(&mut self) {
        td_tpm::zero(self.0.as_mut_slice());
    }
}

impl VolumeKey {
    pub(crate) fn zeroed() -> Self {
        Self(Box::new([0; protocol::VOLUME_KEY_BYTES]))
    }

    /// The buffer a reader fills.
    pub(crate) fn fill(&mut self) -> &mut [u8] {
        self.0.as_mut_slice()
    }

    pub(crate) fn expose(&self) -> &[u8] {
        self.0.as_slice()
    }
}

fn named(path: &Path, what: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{what} {}: {error}", path.display()))
}

fn refused(path: &Path, why: &str) -> io::Error {
    invalid(format!("volume key {}: {why}", path.display()))
}

fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// The key member at `path`: a regular file, not a symlink, of mode 0400
/// owned by `owner`, holding exactly `VOLUME_KEY_BYTES`. Absent is `None`.
fn read_key(path: &Path, owner: u32) -> io::Result<Option<VolumeKey>> {
    let before = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other.map_err(|error| named(path, "inspect volume key", error))?,
    };
    // Nothing but a regular file is opened, so no device node's open runs.
    if !before.file_type().is_file() {
        return Err(refused(path, "not a regular file"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(path)
        .map_err(|error| named(path, "open volume key", error))?;
    let meta = file
        .metadata()
        .map_err(|error| named(path, "stat volume key", error))?;
    if !meta.file_type().is_file() || !same_file(&before, &meta) {
        return Err(refused(path, "changed while it was opened"));
    }
    if meta.mode() & 0o7777 != KEY_MODE {
        return Err(refused(
            path,
            &format!("mode {:04o}, not {KEY_MODE:04o}", meta.mode() & 0o7777),
        ));
    }
    if meta.uid() != owner {
        return Err(refused(path, &format!("owned by uid {}", meta.uid())));
    }
    if meta.len() != protocol::VOLUME_KEY_BYTES as u64 {
        return Err(refused(
            path,
            &format!("{} bytes, not {}", meta.len(), protocol::VOLUME_KEY_BYTES),
        ));
    }
    let mut key = VolumeKey::zeroed();
    let mut reader = &file;
    reader
        .read_exact(key.0.as_mut_slice())
        .map_err(|error| named(path, "read volume key", error))?;
    let mut more = [0u8; 1];
    let extra = reader
        .read(&mut more)
        .map_err(|error| named(path, "read volume key", error))?;
    if extra != 0 {
        td_tpm::zero(&mut more);
        return Err(refused(path, "longer than the key"));
    }
    Ok(Some(key))
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other.map_err(|error| named(path, "remove", error)),
    }
}

/// Takes the handed-off key from the root `root`, whose files are owned by
/// `owner`: reads the member, then removes it and the kernel's
/// `initrd.image` copy whether or not the member was there or passed its
/// checks, so no path leaves either for the system. A refused member or a
/// failed removal refuses; a refused member's bytes were never read.
pub(crate) fn take_key(root: &Path, owner: u32) -> io::Result<Option<VolumeKey>> {
    let member = root.join(protocol::VOLUME_KEY_MEMBER);
    let key = read_key(&member, owner);
    let removed_member = remove(&member);
    let removed_image = remove(&root.join(INITRD_IMAGE));
    removed_member?;
    removed_image?;
    key
}

/// What unlocking a td LUKS2 volume reaches.
pub(crate) trait Unlocker {
    /// Whether a TPM device node exists now; nothing waits for one.
    fn tpm_present(&mut self) -> bool;
    /// The partition's header, through td's bounded reader.
    fn header(&mut self) -> Result<Header, String>;
    /// One td token's unseal on a fresh TPM client.
    fn unseal(&mut self, token: &Token) -> Result<Secret, UnsealError>;
    /// One cryptsetup command (`Cryptsetup::run`).
    fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()>;
    /// The active mapping of the partition that discovery admits.
    fn admit(&mut self) -> io::Result<Option<volume::Mapping>>;
}

/// What the unlock decided.
pub(crate) enum Unlocked {
    /// The opened, admitted and held mapping.
    Mapping(volume::Mapping),
    /// A td token released after the cap, or the check could not show
    /// that none would: the caller halts. The key and any released secret
    /// are already zeroed.
    Halt(String),
}

/// The defence-in-depth unseal after the selector's cap (ENCRYPTION.md
/// "Boot and authority boundaries"). Without a TPM device nothing is
/// attempted. The tokens are the selector's release candidates, so none
/// naming keyslot 0. Each one's unseal must be refused by policy or at
/// load, or find no SHA-256 bank to meet a policy with; a release, or any
/// other outcome, is the halt reason. A header td's reader refuses gives
/// the check no tokens, so nothing is attempted. Console lines are best
/// effort: a failed write must not turn a halt into an exit.
pub(crate) fn check_cap<U: Unlocker>(unlocker: &mut U, out: &mut dyn Write) -> Option<String> {
    if !unlocker.tpm_present() {
        let _ =
            out.write_all(b"td-boot: no TPM device (/dev/tpmrm0): post-cap unseal not attempted\n");
        return None;
    }
    let header = match unlocker.header() {
        Ok(header) => header,
        Err(error) => {
            let _ = out.write_all(
                format!("td-boot: post-cap unseal not attempted: header refused: {error}\n")
                    .as_bytes(),
            );
            return None;
        }
    };
    for (number, token) in release::candidates(&header) {
        let role = token.role().name();
        match unlocker.unseal(token) {
            Ok(secret) => {
                drop(secret);
                return Some(format!(
                    "td token {number} ({role}) unsealed after the PCR 12 release cap; \
                     deployment boot refused"
                ));
            }
            Err(
                error @ (UnsealError::PolicyRefused(_)
                | UnsealError::LoadRefused(_)
                | UnsealError::NoSha256Bank),
            ) => {
                let _ = out.write_all(
                    format!(
                        "td-boot: post-cap unseal of td token {number} ({role}) refused: {error}\n"
                    )
                    .as_bytes(),
                );
            }
            Err(UnsealError::Other(error)) => {
                return Some(format!(
                    "post-cap unseal of td token {number} ({role}) did not show release closed: \
                     {error}; deployment boot refused"
                ));
            }
        }
    }
    None
}

/// Unlocks the partition named to cryptsetup as `device` with `key`: the
/// post-cap check, then `open --volume-key-file` under the fixed mapping
/// name with the key on a descriptor, then discovery's admission of the
/// mapping. The key is zeroed before the mapping is admitted, and on every
/// refusal and halt.
pub(crate) fn unlock<U: Unlocker>(
    unlocker: &mut U,
    device: &Path,
    key: VolumeKey,
    out: &mut dyn Write,
) -> io::Result<Unlocked> {
    if let Some(reason) = check_cap(unlocker, out) {
        drop(key);
        return Ok(Unlocked::Halt(reason));
    }
    {
        let key_file = KeyFile::new(key.expose())?;
        drop(key);
        unlocker.run(
            &cryptsetup::open_by_volume_key_args(
                device,
                protocol::VOLUME_MAPPING_NAME,
                key_file.path(),
            ),
            &[],
        )?;
    }
    match unlocker.admit()? {
        Some(mapping) if mapping.name() == protocol::VOLUME_MAPPING_NAME => {
            Ok(Unlocked::Mapping(mapping))
        }
        Some(mapping) => Err(invalid(format!(
            "the volume's admitted mapping is {:?}, not {:?}",
            mapping.name(),
            protocol::VOLUME_MAPPING_NAME
        ))),
        None => Err(invalid(format!(
            "cryptsetup opened {} but discovery finds no mapping of the volume",
            protocol::VOLUME_MAPPING_NAME
        ))),
    }
}

/// The unlock over the running system: td-tpm's resource manager, the
/// source-built `/bin/cryptsetup` and sysfs.
pub(crate) struct System<'a> {
    partition: &'a volume::Pinned,
    name: String,
    uuid: &'a volume::Uuid,
    cryptsetup: Cryptsetup,
}

impl<'a> System<'a> {
    pub(crate) fn new(partition: &'a volume::Pinned, uuid: &'a volume::Uuid) -> io::Result<Self> {
        let name = partition
            .device
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("missing volume name"))?
            .to_owned();
        Ok(Self {
            partition,
            name,
            uuid,
            cryptsetup: Cryptsetup::new(PathBuf::from(format!("/bin/{}", protocol::CRYPTSETUP))),
        })
    }
}

impl Unlocker for System<'_> {
    fn tpm_present(&mut self) -> bool {
        // Anything but an absent node is a device the check must reach; one
        // that cannot be opened is an unseal that did not answer.
        !matches!(
            fs::symlink_metadata(TPM_DEVICE),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        )
    }

    fn header(&mut self) -> Result<Header, String> {
        let mut file: &File = self.partition.file();
        luks2::read(&mut file)
    }

    fn unseal(&mut self, token: &Token) -> Result<Secret, UnsealError> {
        let client = release::DeviceTpm.open().map_err(UnsealError::Other)?;
        release::unseal_token(client, token)
    }

    fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
        self.cryptsetup.run(args, input)
    }

    fn admit(&mut self) -> io::Result<Option<volume::Mapping>> {
        volume::open_mapping(&self.name, self.uuid)
    }
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
    use std::os::unix::fs::{symlink, PermissionsExt};
    use td_protector::token::Role;
    use td_tpm::SealedObject;

    struct Root(PathBuf);

    impl Root {
        fn new(tag: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("td-boot-unlock-{tag}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn member(&self) -> PathBuf {
            self.0.join(protocol::VOLUME_KEY_MEMBER)
        }

        fn image(&self) -> PathBuf {
            self.0.join(INITRD_IMAGE)
        }

        fn key(&self, bytes: &[u8], mode: u32) {
            fs::write(self.member(), bytes).unwrap();
            fs::set_permissions(self.member(), fs::Permissions::from_mode(mode)).unwrap();
        }

        fn take(&self) -> io::Result<Option<VolumeKey>> {
            take_key(&self.0, own_uid())
        }

        /// Neither the member nor the kernel's copy is left behind.
        fn assert_retired(&self) {
            assert!(fs::symlink_metadata(self.member()).is_err());
            assert!(fs::symlink_metadata(self.image()).is_err());
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The test's own uid stands in for root's, as the owner of what it
    /// creates.
    fn own_uid() -> u32 {
        fs::metadata("/proc/self").unwrap().uid()
    }

    fn key_bytes() -> Vec<u8> {
        (0..protocol::VOLUME_KEY_BYTES as u8)
            .map(|b| b ^ 0xa5)
            .collect()
    }

    #[test]
    fn an_exact_key_is_taken_and_both_copies_removed() {
        let root = Root::new("exact");
        root.key(&key_bytes(), 0o400);
        fs::write(root.image(), b"the whole initrd").unwrap();
        let key = root.take().unwrap().unwrap();
        assert_eq!(key.expose(), key_bytes().as_slice());
        root.assert_retired();
    }

    #[test]
    fn no_key_is_none_and_the_kernel_copy_still_goes() {
        let root = Root::new("absent");
        assert!(root.take().unwrap().is_none());
        fs::write(root.image(), b"the whole initrd").unwrap();
        assert!(root.take().unwrap().is_none());
        root.assert_retired();
    }

    #[test]
    fn a_wrong_member_is_refused_and_still_removed() {
        let wrong: &[(&str, &dyn Fn(&Root))] = &[
            ("mode 0600", &|root| root.key(&key_bytes(), 0o600)),
            ("mode 0440", &|root| root.key(&key_bytes(), 0o440)),
            ("mode 4400", &|root| root.key(&key_bytes(), 0o4400)),
            ("63 bytes", &|root| {
                root.key(&key_bytes()[..protocol::VOLUME_KEY_BYTES - 1], 0o400)
            }),
            ("65 bytes", &|root| {
                let mut bytes = key_bytes();
                bytes.push(0);
                root.key(&bytes, 0o400)
            }),
            ("empty", &|root| root.key(b"", 0o400)),
            ("a symlink", &|root| {
                let target = root.0.join("target");
                fs::write(&target, key_bytes()).unwrap();
                fs::set_permissions(&target, fs::Permissions::from_mode(0o400)).unwrap();
                symlink(&target, root.member()).unwrap();
            }),
        ];
        for (what, place) in wrong {
            let root = Root::new("wrong");
            place(&root);
            fs::write(root.image(), b"the whole initrd").unwrap();
            assert!(root.take().is_err(), "{what} was taken");
            root.assert_retired();
            if *what == "a symlink" {
                // The link is removed, never what it names.
                assert!(root.0.join("target").exists());
            }
        }
    }

    #[test]
    fn a_key_of_another_owner_is_refused_and_removed() {
        let root = Root::new("owner");
        root.key(&key_bytes(), 0o400);
        assert!(take_key(&root.0, own_uid().wrapping_add(1)).is_err());
        root.assert_retired();
    }

    #[test]
    fn a_member_that_is_not_a_file_is_refused() {
        let root = Root::new("dir");
        fs::create_dir(root.member()).unwrap();
        fs::write(root.image(), b"x").unwrap();
        let error = root.take().err().unwrap();
        assert!(error.to_string().contains("remove"), "{error}");
        // A directory cannot be unlinked as a file: the refusal stands, and
        // the kernel's copy is still removed.
        assert!(fs::symlink_metadata(root.image()).is_err());
    }

    fn token(keyslot: u8, role: Role) -> Token {
        Token::new(
            keyslot,
            role,
            SealedObject {
                public: vec![keyslot; 8],
                private: vec![keyslot; 8],
            },
        )
        .unwrap()
    }

    fn header(tokens: Vec<(u8, Token)>) -> Header {
        Header {
            copy: luks2::HeaderCopy::Primary,
            seqid: 1,
            hdr_size: 16384,
            uuid: "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a".into(),
            label: "td-system".into(),
            keyslots: vec![0, 1, 2],
            tokens,
            orphans: Vec::new(),
            foreign: Vec::new(),
        }
    }

    /// What the scripted unseal answers for a token on keyslot N.
    #[derive(Clone)]
    enum Answer {
        Policy,
        Load,
        Bankless,
        Other,
        Release,
    }

    struct Scripted {
        tpm: bool,
        header: Result<Header, String>,
        answers: Vec<Answer>,
        run_fails: bool,
        admit: Option<&'static str>,
        events: Vec<String>,
        key_read: Option<Vec<u8>>,
        argv: Vec<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: &[Answer]) -> Self {
            let tokens = answers
                .iter()
                .enumerate()
                .map(|(index, _)| (index as u8, token(index as u8 + 1, Role::DeviceBound)))
                .collect();
            Self {
                tpm: true,
                header: Ok(header(tokens)),
                answers: answers.to_vec(),
                run_fails: false,
                admit: Some(protocol::VOLUME_MAPPING_NAME),
                events: Vec::new(),
                key_read: None,
                argv: Vec::new(),
            }
        }
    }

    impl Unlocker for Scripted {
        fn tpm_present(&mut self) -> bool {
            self.events.push("tpm".into());
            self.tpm
        }
        fn header(&mut self) -> Result<Header, String> {
            self.events.push("header".into());
            self.header.clone()
        }
        fn unseal(&mut self, token: &Token) -> Result<Secret, UnsealError> {
            self.events.push(format!("unseal {}", token.keyslot()));
            match self.answers[usize::from(token.keyslot()) - 1] {
                Answer::Policy => Err(UnsealError::PolicyRefused("0x1c4".into())),
                Answer::Load => Err(UnsealError::LoadRefused("0x1df".into())),
                Answer::Bankless => Err(UnsealError::NoSha256Bank),
                Answer::Other => Err(UnsealError::Other("no reply".into())),
                Answer::Release => Ok(Secret::generate().unwrap()),
            }
        }
        fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
            let words: Vec<String> = args
                .iter()
                .map(|arg| arg.to_str().unwrap().to_owned())
                .collect();
            self.events.push(format!("run {}", words[0]));
            assert!(input.is_empty(), "nothing on standard input");
            // cryptsetup opens the key file by its name and reads it.
            let at = words.iter().position(|w| w == "--volume-key-file").unwrap();
            let mut read = Vec::new();
            File::open(&words[at + 1])
                .unwrap()
                .read_to_end(&mut read)
                .unwrap();
            self.key_read = Some(read);
            self.argv.push(words);
            if self.run_fails {
                return Err(invalid("cryptsetup open failed (exit status: 2)"));
            }
            Ok(())
        }
        fn admit(&mut self) -> io::Result<Option<volume::Mapping>> {
            self.events.push("admit".into());
            Ok(self.admit.map(|name| {
                volume::Mapping::for_test("dm-0", name, File::open("/dev/null").unwrap())
            }))
        }
    }

    fn key() -> VolumeKey {
        let mut key = VolumeKey::zeroed();
        key.0.copy_from_slice(&key_bytes());
        key
    }

    fn unlock_with(unlocker: &mut Scripted) -> (io::Result<Unlocked>, String) {
        let mut out = Vec::new();
        let result = unlock(unlocker, Path::new("/proc/7/fd/3"), key(), &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    /// The check runs before cryptsetup, cryptsetup before admission, and
    /// the key reaches cryptsetup only through its descriptor.
    #[test]
    fn the_check_precedes_the_open_and_the_key_travels_by_descriptor() {
        let mut unlocker = Scripted::new(&[Answer::Policy, Answer::Load]);
        let (result, out) = unlock_with(&mut unlocker);
        let Unlocked::Mapping(mapping) = result.unwrap() else {
            panic!("halted");
        };
        assert_eq!(mapping.name(), protocol::VOLUME_MAPPING_NAME);
        assert_eq!(
            unlocker.events,
            ["tpm", "header", "unseal 1", "unseal 2", "run open", "admit"]
        );
        assert_eq!(
            out.matches("post-cap unseal of td token").count(),
            2,
            "{out}"
        );
        assert_eq!(unlocker.key_read.as_deref(), Some(key_bytes().as_slice()));
        let argv = &unlocker.argv[0];
        let prefix = format!("/proc/{}/fd/", std::process::id());
        assert_eq!(argv[..4], ["open", "--type", "luks2", "--volume-key-file"]);
        assert!(argv[4].starts_with(&prefix), "{argv:?}");
        assert_eq!(argv[5..], ["/proc/7/fd/3", protocol::VOLUME_MAPPING_NAME]);
    }

    #[test]
    fn without_a_tpm_device_nothing_is_attempted() {
        let mut unlocker = Scripted::new(&[Answer::Release]);
        unlocker.tpm = false;
        let (result, out) = unlock_with(&mut unlocker);
        assert!(matches!(result.unwrap(), Unlocked::Mapping(_)));
        assert_eq!(unlocker.events, ["tpm", "run open", "admit"]);
        assert!(out.contains("not attempted"), "{out}");
    }

    #[test]
    fn a_header_td_refuses_attempts_nothing_and_opens() {
        let mut unlocker = Scripted::new(&[Answer::Release]);
        unlocker.header = Err("copies disagree".into());
        let (result, out) = unlock_with(&mut unlocker);
        assert!(matches!(result.unwrap(), Unlocked::Mapping(_)));
        assert_eq!(unlocker.events, ["tpm", "header", "run open", "admit"]);
        assert!(out.contains("copies disagree"), "{out}");
    }

    /// A release, or any outcome but a policy or load refusal, halts before
    /// cryptsetup runs, and no later token is tried.
    #[test]
    fn a_release_or_an_unanswered_unseal_halts_before_cryptsetup() {
        for (answers, tried) in [
            (vec![Answer::Release, Answer::Policy], 1),
            (vec![Answer::Policy, Answer::Release, Answer::Load], 2),
            (vec![Answer::Other, Answer::Policy], 1),
            (vec![Answer::Load, Answer::Other], 2),
        ] {
            let mut unlocker = Scripted::new(&answers);
            let (result, _) = unlock_with(&mut unlocker);
            let Unlocked::Halt(reason) = result.unwrap() else {
                panic!("did not halt");
            };
            assert!(reason.contains("refused"), "{reason}");
            let unseals = unlocker
                .events
                .iter()
                .filter(|event| event.starts_with("unseal"))
                .count();
            assert_eq!(unseals, tried);
            assert!(unlocker
                .events
                .iter()
                .all(|event| !event.starts_with("run")));
            assert!(unlocker.key_read.is_none());
        }
    }

    #[test]
    fn every_policy_or_load_refusal_proceeds() {
        let mut unlocker =
            Scripted::new(&[Answer::Policy, Answer::Load, Answer::Bankless, Answer::Load]);
        let (result, _) = unlock_with(&mut unlocker);
        assert!(matches!(result.unwrap(), Unlocked::Mapping(_)));
        assert_eq!(
            unlocker
                .events
                .iter()
                .filter(|event| event.starts_with("unseal"))
                .count(),
            4
        );
    }

    /// The tokens tried are the selector's candidates: device-bound first,
    /// then first-boot, each by number, never one naming keyslot 0.
    #[test]
    fn the_check_tries_the_selectors_candidates_only() {
        let mut unlocker = Scripted::new(&[Answer::Policy, Answer::Policy, Answer::Load]);
        unlocker.header = Ok(header(vec![
            (0, token(1, Role::FirstBoot)),
            (1, token(0, Role::DeviceBound)),
            (2, token(2, Role::DeviceBound)),
            (3, token(3, Role::FirstBoot)),
        ]));
        let (result, _) = unlock_with(&mut unlocker);
        assert!(matches!(result.unwrap(), Unlocked::Mapping(_)));
        assert_eq!(
            unlocker.events,
            ["tpm", "header", "unseal 2", "unseal 1", "unseal 3", "run open", "admit"]
        );
    }

    /// A TPM answering PCR_Read with no SHA-256 bank, through td-protector's
    /// own release-policy read: the unseal is typed `NoSha256Bank`, which
    /// releases nothing, rather than a halt.
    struct Bankless(std::rc::Rc<std::cell::Cell<usize>>);

    impl td_tpm::Transport for Bankless {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            self.0.set(self.0.get() + 1);
            assert_eq!(command.get(6..10), Some([0, 0, 1, 0x7e].as_slice()));
            let mut reply = vec![0x80, 1, 0, 0, 0, 0, 0, 0, 0, 0];
            reply.extend_from_slice(&[0, 0, 0, 0x55, 0, 0, 0, 1, 0, 0x0b, 3, 0, 0, 0, 0, 0, 0, 0]);
            let size = reply.len() as u8;
            reply[5] = size;
            Ok(reply)
        }
    }

    #[test]
    fn a_bankless_tpm_releases_nothing_through_the_real_unseal() {
        let exchanges = std::rc::Rc::new(std::cell::Cell::new(0));
        match release::unseal_token(
            td_tpm::Client::new(Bankless(exchanges.clone())),
            &token(1, Role::DeviceBound),
        ) {
            Err(error) => assert_eq!(error, UnsealError::NoSha256Bank),
            Ok(_) => panic!("released without a SHA-256 bank"),
        }
        assert_eq!(exchanges.get(), 1);
        let mut unlocker = Scripted::new(&[Answer::Bankless, Answer::Bankless]);
        let (result, out) = unlock_with(&mut unlocker);
        assert!(matches!(result.unwrap(), Unlocked::Mapping(_)));
        assert_eq!(out.matches("no SHA-256 PCR bank").count(), 2, "{out}");
    }

    #[test]
    fn a_failed_open_or_an_unadmitted_mapping_refuses() {
        let mut unlocker = Scripted::new(&[]);
        unlocker.run_fails = true;
        let (result, _) = unlock_with(&mut unlocker);
        assert!(result.is_err());
        assert_eq!(unlocker.events, ["tpm", "header", "run open"]);

        let mut unlocker = Scripted::new(&[]);
        unlocker.admit = None;
        let error = unlock_with(&mut unlocker).0.err().unwrap();
        assert!(error.to_string().contains("no mapping"), "{error}");

        let mut unlocker = Scripted::new(&[]);
        unlocker.admit = Some("td-other");
        let error = unlock_with(&mut unlocker).0.err().unwrap();
        assert!(error.to_string().contains("td-other"), "{error}");
    }

    #[test]
    fn the_production_unlocker_names_the_shipped_programs() {
        let source = include_str!("unlock.rs");
        let (production, _) = source.split_once("\n#[cfg(test)]\n").unwrap();
        assert!(production.contains("format!(\"/bin/{}\", protocol::CRYPTSETUP)"));
        assert!(production.contains("const TPM_DEVICE: &str = \"/dev/tpmrm0\";"));
        assert_eq!(protocol::VOLUME_KEY_MEMBER, "td-volume-key-v1");
        assert_eq!(protocol::VOLUME_KEY_BYTES, 64);
        assert_eq!(protocol::VOLUME_MAPPING_NAME, "td-system");
    }
}
