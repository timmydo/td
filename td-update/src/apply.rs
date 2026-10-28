//! Privileged mechanism for one consent-bound installation, with no elevation.

use crate::{io, Result, O_NOFOLLOW, O_NONBLOCK};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// Root-private staging this operation owns; nothing else is kept there.
const STATE: &str = "/var/lib/td-update";
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
    let parent = path
        .parent()
        .ok_or_else(|| String::from("update staging has no parent"))?;
    for ancestor in parent.ancestors() {
        let metadata = io(
            fs::symlink_metadata(ancestor),
            &format!("inspect update staging path {}", ancestor.display()),
        )?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err("update staging needs real root-owned trusted ancestors".into());
        }
    }
    let created = match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(format!("create update staging: {error}")),
    };
    let directory = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(path),
        "open update staging",
    )?;
    // A creation mode is masked by the umask; the descriptor's is exact.
    if created {
        io(
            directory.set_permissions(fs::Permissions::from_mode(0o700)),
            "protect update staging",
        )?;
    }
    validate_private_state(&directory, 0)?;
    Ok(directory)
}

fn validate_private_state(directory: &File, owner: u32) -> Result<()> {
    let state = io(directory.metadata(), "inspect update staging")?;
    if !state.is_dir() || state.uid() != owner || state.mode() & 0o7777 != 0o700 {
        return Err("update staging must be an owned 0700 directory".into());
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
    Validate,
    Install,
}

fn apply(
    source: &File,
    expected: &str,
    state: &Path,
    requester: u32,
    execute: &mut impl FnMut(Step, &Path) -> Result<Vec<u8>>,
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
    // The staged copy must be the approved deployment as td-boot reads it.
    let validated = execute(Step::Validate, &staging.path.0)?;
    if validated.strip_suffix(b"\n") != Some(expected.as_bytes()) {
        return Err("staged deployment is not the approved deployment".into());
    }
    // td-boot rechecks all payload digests inside its existing serialized
    // transaction before publishing current/previous selectors, and reports
    // what it published, which must be the approved deployment too.
    let installed = execute(Step::Install, &staging.path.0)?;
    if installed.strip_suffix(b"\n") != Some(expected.as_bytes()) {
        return Err("published deployment is not the approved deployment".into());
    }
    Ok(())
}

fn execute(step: Step, source: &Path) -> Result<Vec<u8>> {
    let mut command = Command::new("/bin/td-boot");
    match step {
        Step::Validate => command.arg("validate-source").arg(source),
        Step::Install => command
            .args(["on-volume", "install", "/run/td-update"])
            .arg(source),
    };
    let output = io(
        command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output(),
        "run installation operation",
    )?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!("installation {step:?} failed ({})", output.status))
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
        fn apply(&self, executor: &mut impl FnMut(Step, &Path) -> Result<Vec<u8>>) -> Result<()> {
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
    fn staging_state_refuses_exposure() {
        let fixture = Fixture::new();
        let state_path = fixture.root.join("state");
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o700)).unwrap();
        let state = File::open(&state_path).unwrap();
        validate_private_state(&state, fixture.owner).unwrap();
        for mode in [0o755, 0o2700, 0o500] {
            state
                .set_permissions(fs::Permissions::from_mode(mode))
                .unwrap();
            assert!(validate_private_state(&state, fixture.owner).is_err());
        }
        state
            .set_permissions(fs::Permissions::from_mode(0o700))
            .unwrap();
        assert!(validate_private_state(&state, fixture.owner + 1).is_err());
        let file = File::create(fixture.root.join("file")).unwrap();
        assert!(validate_private_state(&file, fixture.owner).is_err());
    }

    #[test]
    fn approval_binds_manifest_and_source_descriptor_before_privileged_work() {
        let fixture = Fixture::new();
        fs::rename(fixture.root.join("source"), fixture.root.join("moved")).unwrap();
        fs::create_dir(fixture.root.join("source")).unwrap();
        fs::write(fixture.root.join("source/manifest"), b"replacement").unwrap();
        let mut calls = Vec::new();
        let id = format!("{}\n", fixture.expected).into_bytes();
        fixture
            .apply(&mut |step, staged| {
                calls.push(step);
                assert_eq!(fs::metadata(staged).unwrap().mode() & 0o7777, 0o700);
                for (name, _) in PAYLOADS {
                    let path = staged.join(name);
                    assert_eq!(fs::read(&path).unwrap(), name.as_bytes());
                    assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o600);
                }
                assert!(!staged.join("manifest.sig").exists());
                Ok(id.clone())
            })
            .unwrap();
        assert_eq!(calls, [Step::Validate, Step::Install]);
        fixture.empty();
        fs::write(
            fixture.root.join("moved/manifest"),
            b"changed approved bytes",
        )
        .unwrap();
        assert!(fixture
            .apply(&mut |_, _| panic!("changed manifest reached td-boot"))
            .is_err());
        fixture.empty();
    }

    #[test]
    fn a_validation_naming_another_deployment_never_reaches_publication() {
        for answer in [
            format!("{}\n", "0".repeat(64)),
            String::new(),
            "not an id\n".to_string(),
        ] {
            let fixture = Fixture::new();
            // Without its newline the approved id is not td-boot's report.
            let bare = fixture.expected.clone();
            for reported in [answer.clone(), bare] {
                let mut calls = Vec::new();
                assert!(fixture
                    .apply(&mut |step, _| {
                        calls.push(step);
                        Ok(reported.clone().into_bytes())
                    })
                    .is_err());
                assert_eq!(calls, [Step::Validate]);
                fixture.empty();
            }
        }
    }

    #[test]
    fn an_install_reporting_another_deployment_fails() {
        for answer in [
            format!("{}\n", "0".repeat(64)),
            String::new(),
            "not an id\n".to_string(),
        ] {
            let fixture = Fixture::new();
            let id = format!("{}\n", fixture.expected);
            let mut calls = Vec::new();
            let error = fixture
                .apply(&mut |step, _| {
                    calls.push(step);
                    Ok(match step {
                        Step::Validate => id.clone(),
                        Step::Install => answer.clone(),
                    }
                    .into_bytes())
                })
                .unwrap_err();
            assert!(error.contains("published deployment"), "{error}");
            assert_eq!(calls, [Step::Validate, Step::Install]);
            fixture.empty();
        }
    }

    #[test]
    fn failed_validation_never_reaches_publication() {
        for fail in [Step::Validate, Step::Install] {
            let fixture = Fixture::new();
            let id = format!("{}\n", fixture.expected).into_bytes();
            let mut calls = Vec::new();
            assert!(fixture
                .apply(&mut |step, _| {
                    calls.push(step);
                    if step == fail {
                        Err("injected failure".into())
                    } else {
                        Ok(id.clone())
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
            .apply(&mut |_, _| panic!("symlink reached td-boot"))
            .is_err());
        fixture.empty();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(fixture
            .apply(&mut |_, _| panic!("directory reached td-boot"))
            .is_err());
        fs::remove_dir(&path).unwrap();
        File::create(&path)
            .unwrap()
            .set_len((256 << 20) + 1)
            .unwrap();
        assert!(fixture
            .apply(&mut |_, _| panic!("oversized input reached td-boot"))
            .is_err());
        fixture.empty();
    }
}
