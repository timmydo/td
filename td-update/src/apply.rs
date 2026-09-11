//! Privileged mechanism for one consent-bound installation, with no elevation.

use crate::{io, Result, O_NOFOLLOW, O_NONBLOCK};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const STATE: &str = "/var/lib/td-deploy";
const TRUST: &str = "/run/td-volume/td/trusted.pub";
const O_PATH: i32 = 0x200000; // Linux x86-64, safe std open flag.
const PAYLOADS: [(&str, u64); 3] = [
    ("bzImage", 256 << 20),
    ("initramfs.cpio", 512 << 20),
    ("root.erofs", 128 << 30),
];

pub(crate) fn run(expected: &str) -> Result<()> {
    if io(fs::metadata("/proc/self"), "read caller identity")?.uid() != 0 {
        return Err("apply-operation requires the installation authority".into());
    }
    // The authority supplies the directory itself through stdin, not a pathname
    // that the requester could replace after the consent description is pinned.
    let source = File::from(io(
        std::io::stdin().as_fd().try_clone_to_owned(),
        "retain approved source directory",
    )?);
    let _state = private_state(Path::new(STATE))?;
    apply(&source, expected, Path::new(STATE), 1000, &mut execute)
}

fn descriptor(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn regular(path: &Path, owner: u32, limit: u64) -> Result<File> {
    let file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(path),
        "open deployment input",
    )?;
    let metadata = io(file.metadata(), "inspect deployment input")?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.len() == 0
        || metadata.len() > limit
    {
        return Err(format!(
            "input must be a bounded regular file owned by UID {owner}"
        ));
    }
    Ok(file)
}

fn private_state(path: &Path) -> Result<File> {
    // This fixed installation path is never created or repaired by an update.
    // In particular, losing the signing identity must not generate a new one.
    for ancestor in path.ancestors() {
        let metadata = io(
            fs::symlink_metadata(ancestor),
            &format!("inspect installation state path {}", ancestor.display()),
        )?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err("installation state needs real root-owned trusted ancestors".into());
        }
    }
    let directory = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(path),
        "open installation state",
    )?;
    let key = regular(&descriptor(&directory).join("deployment.pk8"), 0, 16384)
        .map_err(|e| format!("installation signing key: {e}"))?;
    validate_private_metadata(&directory, &key, 0)?;
    Ok(directory)
}

fn validate_private_metadata(directory: &File, key: &File, owner: u32) -> Result<()> {
    let state = io(directory.metadata(), "inspect installation state")?;
    if !state.is_dir() || state.uid() != owner || state.mode() & 0o7777 != 0o700 {
        return Err("installation signing state must be an owned 0700 directory".into());
    }
    let metadata = io(key.metadata(), "inspect installation key")?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.len() == 0
        || metadata.len() > 16384
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err("installation key must be an owned bounded single-link 0600 file".into());
    }
    Ok(())
}

struct CreatedDirectory(PathBuf);
impl Drop for CreatedDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Staging {
    directory: File,
    path: CreatedDirectory,
}
impl Staging {
    fn create(state: &Path) -> Result<Self> {
        let mut nonce = [0; 16];
        io(
            File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut nonce)),
            "read update staging randomness",
        )?;
        let path = state.join(format!(".update-{}", crate::sha256::hex_digest(&nonce)));
        io(
            fs::DirBuilder::new().mode(0o700).create(&path),
            "create private update staging",
        )?;
        let path = CreatedDirectory(path);
        let pinned = io(
            OpenOptions::new()
                .read(true)
                .custom_flags(O_PATH | O_NOFOLLOW)
                .open(&path.0),
            "pin private update staging",
        )?;
        io(
            fs::set_permissions(descriptor(&pinned), fs::Permissions::from_mode(0o700)),
            "protect private update staging",
        )?;
        let directory = io(
            File::open(descriptor(&pinned)),
            "open private update staging",
        )?;
        Ok(Self { directory, path })
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = io(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path),
        "create staged deployment file",
    )?;
    io(
        file.set_permissions(fs::Permissions::from_mode(0o600)),
        "protect staged deployment file",
    )?;
    io(
        file.write_all(bytes).and_then(|()| file.sync_all()),
        "persist staged deployment file",
    )
}

fn copy_input(mut input: File, destination: &Path, limit: u64) -> Result<()> {
    let length = io(input.metadata(), "read deployment payload size")?.len();
    if length == 0 || length > limit {
        return Err("deployment payload size changed".into());
    }
    let mut output = io(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination),
        "create staged deployment payload",
    )?;
    io(
        output.set_permissions(fs::Permissions::from_mode(0o600)),
        "protect staged deployment payload",
    )?;
    let copied = io(
        std::io::copy(&mut (&mut input).take(length + 1), &mut output),
        "copy deployment payload",
    )?;
    if copied != length {
        return Err("deployment payload changed size while staging".into());
    }
    io(output.sync_all(), "persist staged deployment payload")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Sign,
    Authenticate,
    Install,
}

fn apply(
    source: &File,
    expected: &str,
    state: &Path,
    requester: u32,
    execute: &mut impl FnMut(Step, &Path, &Path) -> Result<()>,
) -> Result<()> {
    if !crate::protocol::valid_digest(expected.as_bytes()) {
        return Err("invalid approved deployment ID".into());
    }
    let metadata = io(source.metadata(), "inspect approved source directory")?;
    if !metadata.is_dir() || metadata.uid() != requester {
        return Err("approved source directory must be owned by the requester".into());
    }
    let input = descriptor(source);
    let mut manifest = Vec::new();
    io(
        regular(
            &input.join("manifest"),
            requester,
            crate::protocol::MAX_MANIFEST_BYTES,
        )?
        .take(crate::protocol::MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut manifest),
        "read approved manifest",
    )?;
    if manifest.len() as u64 > crate::protocol::MAX_MANIFEST_BYTES
        || crate::sha256::hex_digest(&manifest) != expected
    {
        return Err("source manifest differs from the approved deployment".into());
    }
    // Pin every payload before creating privileged state. Only these fixed
    // names may cross from the user's build into a system deployment.
    let mut inputs = Vec::new();
    for (name, limit) in PAYLOADS {
        inputs.push((name, limit, regular(&input.join(name), requester, limit)?));
    }
    let staging = Staging::create(state)?;
    let output = descriptor(&staging.directory);
    write_new(&output.join("manifest"), &manifest)?;
    for (name, limit, file) in inputs {
        copy_input(file, &output.join(name), limit)?;
    }
    io(
        staging.directory.sync_all(),
        "persist private update directory",
    )?;
    execute(Step::Sign, &staging.path.0, state)?;
    execute(Step::Authenticate, &staging.path.0, state)?;
    // td-boot rechecks the signature and all payload digests inside its existing
    // serialized transaction before publishing current/previous selectors.
    execute(Step::Install, &staging.path.0, state)
}

fn execute(step: Step, source: &Path, state: &Path) -> Result<()> {
    let mut command = match step {
        Step::Sign => {
            let mut command = Command::new("/bin/td-deploy");
            command
                .arg("sign")
                .arg(source.join("manifest"))
                .arg(state.join("deployment.pk8"))
                .arg(source.join("manifest.sig"));
            command
        }
        Step::Authenticate => {
            let mut command = Command::new("/bin/td-boot");
            command.arg("authenticate").arg(source).arg(TRUST);
            command
        }
        Step::Install => {
            let mut command = Command::new("/bin/td-boot");
            command
                .args(["install", "/dev/vda", "/run/td-update"])
                .arg(source)
                .arg(TRUST);
            command
        }
    };
    command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let status = io(command.status(), "run installation operation")?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("installation {step:?} failed ({status})"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture {
        root: PathBuf,
        source: File,
        expected: String,
        owner: u32,
    }
    impl Fixture {
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "td-apply-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            fs::create_dir(root.join("source")).unwrap();
            fs::create_dir(root.join("state")).unwrap();
            let body = b"td-deployment-v1\nfixture\n";
            fs::write(root.join("source/manifest"), body).unwrap();
            for (name, _) in PAYLOADS {
                fs::write(root.join("source").join(name), name.as_bytes()).unwrap();
            }
            let source = File::open(root.join("source")).unwrap();
            let owner = source.metadata().unwrap().uid();
            Self {
                root,
                source,
                expected: crate::sha256::hex_digest(body),
                owner,
            }
        }
        fn apply(&self, executor: &mut impl FnMut(Step, &Path, &Path) -> Result<()>) -> Result<()> {
            apply(
                &self.source,
                &self.expected,
                &self.root.join("state"),
                self.owner,
                executor,
            )
        }
        fn empty(&self) {
            assert_eq!(fs::read_dir(self.root.join("state")).unwrap().count(), 0);
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn retained_signing_state_refuses_exposure_aliases_and_damaged_keys() {
        let fixture = Fixture::new();
        let state_path = fixture.root.join("state");
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o700)).unwrap();
        let state = File::open(&state_path).unwrap();
        let key_path = state_path.join("deployment.pk8");
        fs::write(&key_path, b"retained key").unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        let key = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&key_path)
            .unwrap();
        validate_private_metadata(&state, &key, fixture.owner).unwrap();
        for mode in [0o644, 0o4600] {
            key.set_permissions(fs::Permissions::from_mode(mode))
                .unwrap();
            assert!(validate_private_metadata(&state, &key, fixture.owner).is_err());
        }
        key.set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        for mode in [0o755, 0o2700] {
            state
                .set_permissions(fs::Permissions::from_mode(mode))
                .unwrap();
            assert!(validate_private_metadata(&state, &key, fixture.owner).is_err());
        }
        state
            .set_permissions(fs::Permissions::from_mode(0o700))
            .unwrap();
        let alias = state_path.join("alias");
        fs::hard_link(&key_path, &alias).unwrap();
        assert!(validate_private_metadata(&state, &key, fixture.owner).is_err());
        fs::remove_file(alias).unwrap();
        for size in [0, 16385] {
            key.set_len(size).unwrap();
            assert!(validate_private_metadata(&state, &key, fixture.owner).is_err());
        }
    }

    #[test]
    fn approval_binds_manifest_and_source_descriptor_before_privileged_work() {
        let fixture = Fixture::new();
        fs::rename(fixture.root.join("source"), fixture.root.join("moved")).unwrap();
        fs::create_dir(fixture.root.join("source")).unwrap();
        fs::write(fixture.root.join("source/manifest"), b"replacement").unwrap();
        let mut calls = Vec::new();
        fixture
            .apply(&mut |step, staged, _| {
                calls.push(step);
                assert_eq!(fs::metadata(staged).unwrap().mode() & 0o7777, 0o700);
                for (name, _) in PAYLOADS {
                    let path = staged.join(name);
                    assert_eq!(fs::read(&path).unwrap(), name.as_bytes());
                    assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o600);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, [Step::Sign, Step::Authenticate, Step::Install]);
        fixture.empty();
        fs::write(
            fixture.root.join("moved/manifest"),
            b"changed approved bytes",
        )
        .unwrap();
        assert!(fixture
            .apply(&mut |_, _, _| panic!("changed manifest reached signer"))
            .is_err());
        fixture.empty();
    }

    #[test]
    fn failed_signing_or_authentication_never_reaches_publication() {
        for fail in [Step::Sign, Step::Authenticate, Step::Install] {
            let fixture = Fixture::new();
            let mut calls = Vec::new();
            assert!(fixture
                .apply(&mut |step, _, _| {
                    calls.push(step);
                    if step == fail {
                        Err("injected failure".into())
                    } else {
                        Ok(())
                    }
                })
                .is_err());
            assert_eq!(calls.last(), Some(&fail));
            assert_eq!(
                calls.iter().filter(|&&step| step == Step::Install).count(),
                usize::from(fail == Step::Install)
            );
            fixture.empty();
        }
    }

    #[test]
    fn symlink_special_and_oversized_inputs_refuse_before_staging() {
        let fixture = Fixture::new();
        let path = fixture.root.join("source/bzImage");
        fs::remove_file(&path).unwrap();
        symlink("/dev/zero", &path).unwrap();
        assert!(fixture
            .apply(&mut |_, _, _| panic!("symlink reached signer"))
            .is_err());
        fixture.empty();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(fixture
            .apply(&mut |_, _, _| panic!("directory reached signer"))
            .is_err());
        fs::remove_dir(&path).unwrap();
        File::create(&path)
            .unwrap()
            .set_len((256 << 20) + 1)
            .unwrap();
        assert!(fixture
            .apply(&mut |_, _, _| panic!("oversized input reached signer"))
            .is_err());
        fixture.empty();
    }
}
