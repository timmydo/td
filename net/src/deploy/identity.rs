//! Durable installation identity. The enclosing installer owns authorization.
use crate::sig::{keygen, to_hex};
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

// Linux x86-64, the distribution target. These are safe std open flags.
const PATH_ONLY: i32 = 0x200000;
const NOFOLLOW: i32 = 0x20000;
const NONBLOCK: i32 = 0x800;
const KEY: &str = "deployment.pk8";
const TEMP: &str = ".deployment.pk8.new";
const MAX_KEY: u64 = 16384;
type Result<T> = std::result::Result<T, String>;

fn io<T>(value: std::io::Result<T>, action: &str) -> Result<T> {
    value.map_err(|error| format!("{action}: {error}"))
}

fn private_file(file: &File, uid: u32) -> Result<()> {
    let m = io(file.metadata(), "inspect installation identity file")?;
    if !m.is_file() || m.uid() != uid || m.nlink() != 1 || m.mode() & 0o7777 != 0o600 {
        return Err(
            "installation identity files must be owned regular files at 0600 with one link".into(),
        );
    }
    Ok(())
}

fn state_directory(path: &Path, uid: u32) -> Result<File> {
    if path.starts_with("/td/store") {
        return Err("installation signing keys must remain outside /td/store".into());
    }
    if !path.is_absolute() || path.file_name().is_none() {
        return Err("installation identity requires an absolute state directory".into());
    }
    let mut ancestor = PathBuf::new();
    let parent = path.parent().ok_or("installation state has no parent")?;
    for component in parent.components() {
        if !matches!(component, Component::RootDir | Component::Normal(_)) {
            return Err("installation state path must not contain dot components".into());
        }
        ancestor.push(component.as_os_str());
        let m = io(
            fs::symlink_metadata(&ancestor),
            "inspect installation state ancestor",
        )?;
        if !m.is_dir()
            || (m.uid() != 0 && m.uid() != uid)
            || (m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0)
        {
            return Err("installation state ancestors must be trusted directories; shared ancestors require the sticky bit".into());
        }
    }
    let created = match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(format!("create installation identity state: {error}")),
    };
    let pinned = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(PATH_ONLY | NOFOLLOW)
            .open(path),
        "open installation identity state",
    )?;
    let m = io(pinned.metadata(), "inspect installation identity state")?;
    if !m.is_dir() || m.uid() != uid || (!created && m.mode() & 0o7777 != 0o700) {
        return Err("installation identity state must be an owned directory at 0700".into());
    }
    let descriptor = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
    if created {
        // O_PATH pins even a directory made unreadable by the caller's umask.
        io(
            fs::set_permissions(&descriptor, fs::Permissions::from_mode(0o700)),
            "set new installation state permissions",
        )?;
    }
    io(
        File::open(descriptor),
        "open pinned installation identity state",
    )
}

fn public_key(file: &mut File, uid: u32) -> Result<String> {
    private_file(file, uid)?;
    let mut bytes = Vec::new();
    io(
        (&mut *file).take(MAX_KEY + 1).read_to_end(&mut bytes),
        "read installation signing key",
    )?;
    if bytes.len() as u64 > MAX_KEY {
        return Err("installation signing key is oversized".into());
    }
    let key = Ed25519KeyPair::from_pkcs8(&bytes)
        .map_err(|_| "installation signing key is invalid; refusing to replace it")?;
    io(file.sync_all(), "sync retained installation signing key")?;
    Ok(format!("{}\n", to_hex(key.public_key().as_ref())))
}

/// Print only the public half after the private key and its name are durable.
pub(super) fn provision(path: &Path) -> Result<String> {
    // Linux follows a final symlink with a trailing slash or dot even when
    // O_NOFOLLOW is set. Strip those decorations before all path operations.
    let normalized: PathBuf = path.components().collect();
    let path = normalized.as_path();
    let uid = io(fs::metadata("/proc/self"), "read installation identity UID")?.uid();
    let directory = state_directory(path, uid)?;
    // Keep all child lookup on the directory we validated and hold open.
    let root = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let lock_path = root.join("identity.lock");
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW | NONBLOCK);
    let lock = match options.create_new(true).open(&lock_path) {
        Ok(file) => {
            io(
                file.set_permissions(fs::Permissions::from_mode(0o600)),
                "set new installation lock permissions",
            )?;
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => io(
            options.create_new(false).open(&lock_path),
            "open installation identity lock",
        )?,
        Err(error) => return Err(format!("create installation identity lock: {error}")),
    };
    private_file(&lock, uid)?;
    io(lock.lock(), "lock installation identity")?;
    let key_path = root.join(KEY);
    let existing = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(&key_path);
    let public = match existing {
        Ok(mut file) => public_key(&mut file, uid)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::remove_file(root.join(TEMP)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("remove unpublished installation key: {error}")),
            }
            let (private, _) = keygen()?;
            let mut file = io(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(root.join(TEMP)),
                "create installation signing key",
            )?;
            io(
                file.set_permissions(fs::Permissions::from_mode(0o600)),
                "set new installation key permissions",
            )?;
            private_file(&file, uid)?;
            io(file.write_all(&private), "write installation signing key")?;
            io(file.sync_all(), "sync installation signing key")?;
            drop(file);
            io(
                fs::rename(root.join(TEMP), &key_path),
                "publish installation signing key",
            )?;
            let mut readback = io(
                OpenOptions::new()
                    .read(true)
                    .custom_flags(NOFOLLOW | NONBLOCK)
                    .open(&key_path),
                "read back installation signing key",
            )?;
            public_key(&mut readback, uid)?
        }
        Err(error) => return Err(format!("open installation signing key: {error}")),
    };
    io(directory.sync_all(), "sync installation identity directory")?;
    let now = io(
        fs::symlink_metadata(path),
        "check installation identity publication",
    )?;
    let held = io(
        directory.metadata(),
        "check installation identity descriptor",
    )?;
    if (now.dev(), now.ino()) != (held.dev(), held.ino()) || !now.is_dir() {
        return Err("installation identity directory moved during provisioning".into());
    }
    // A newly created state directory also needs its entry persisted.
    let parent = path.parent().ok_or("installation state has no parent")?;
    io(
        OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(parent)
            .and_then(|f| f.sync_all()),
        "sync installation identity parent",
    )?;
    Ok(public)
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
    use crate::sig::{from_hex, sign_msg, verify_msg};
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    #[test]
    fn identity_contract_in_trusted_namespace() {
        // Hosted checks have unmapped host-root owners. Use the repository's
        // ownership fixture, keeping production's admission checks intact.
        let builder = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/release/td-builder");
        let output = std::process::Command::new(builder)
            .arg("run-capped")
            .arg(std::env::current_exe().unwrap())
            .args([
                "deploy::identity::tests::",
                "--include-ignored",
                "--skip",
                "deploy::identity::tests::identity_contract_in_trusted_namespace",
            ])
            .env("TD_TEST_TRUSTED_ROOT", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("6 passed; 0 failed; 0 ignored"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-identity-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[ignore = "run by identity_contract_in_trusted_namespace"]
    fn identities_are_unique_durable_and_sign_with_the_reported_key() {
        let scratch = Scratch::new();
        let a = scratch.0.join("a");
        let b = scratch.0.join("b");
        let public = provision(&a).unwrap();
        let private = fs::read(a.join(KEY)).unwrap();
        assert_eq!(public, provision(&a).unwrap());
        assert_eq!(private, fs::read(a.join(KEY)).unwrap());
        assert_ne!(public, provision(&b).unwrap());
        assert_eq!(fs::metadata(a.join(KEY)).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&a).unwrap().mode() & 0o777, 0o700);
        let message = b"td-deployment-v1\n";
        let signature = sign_msg(&private, message).unwrap();
        assert!(verify_msg(&from_hex(&public).unwrap(), message, &signature));
        assert!(!verify_msg(
            &from_hex(&provision(&b).unwrap()).unwrap(),
            message,
            &signature
        ));
    }

    #[test]
    #[ignore = "run by identity_contract_in_trusted_namespace"]
    fn concurrent_provisioning_retains_one_identity() {
        let scratch = Scratch::new();
        let state = scratch.0.join("state");
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let state = state.clone();
                std::thread::spawn(move || provision(&state).unwrap())
            })
            .collect();
        let keys: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(keys.iter().all(|key| Some(key) == keys.first()));
    }

    #[test]
    #[ignore = "run by identity_contract_in_trusted_namespace"]
    fn interrupted_unpublished_keys_retry_but_published_damage_never_rotates() {
        let scratch = Scratch::new();
        let state = scratch.0.join("state");
        DirBuilder::new().mode(0o700).create(&state).unwrap();
        fs::write(state.join(TEMP), b"partial").unwrap();
        provision(&state).unwrap();
        assert!(!state.join(TEMP).exists());
        fs::write(state.join(KEY), b"damaged").unwrap();
        assert!(provision(&state)
            .unwrap_err()
            .contains("refusing to replace"));
        assert_eq!(fs::read(state.join(KEY)).unwrap(), b"damaged");
    }

    #[test]
    #[ignore = "run by identity_contract_in_trusted_namespace"]
    fn decorated_state_symlinks_are_refused_without_creating_identity() {
        let scratch = Scratch::new();
        let target = scratch.0.join("target");
        DirBuilder::new().mode(0o700).create(&target).unwrap();
        let link = scratch.0.join("link");
        symlink(&target, &link).unwrap();
        for suffix in ["", "/", "/."] {
            let mut spelling = link.as_os_str().to_os_string();
            spelling.push(suffix);
            assert!(provision(Path::new(&spelling)).is_err());
            assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        }
    }

    #[test]
    #[ignore = "run by identity_contract_in_trusted_namespace"]
    fn restrictive_umask_preserves_new_object_permissions() {
        const CHILD: &str = "TD_IDENTITY_UMASK_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let new = root.join("new");
            let public = provision(&new).unwrap();
            assert_eq!(public, provision(&new).unwrap());
            // Cover first key/lock creation in a pre-existing state as well.
            provision(&root.join("existing")).unwrap();
            for path in [new, root.join("existing")] {
                assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
                for name in [KEY, "identity.lock"] {
                    assert_eq!(
                        fs::metadata(path.join(name)).unwrap().mode() & 0o7777,
                        0o600
                    );
                }
            }
            return;
        }
        for mask in ["0777", "0277"] {
            let scratch = Scratch::new();
            DirBuilder::new()
                .mode(0o700)
                .create(scratch.0.join("existing"))
                .unwrap();
            // Set the umask only in a child; no process-global unsafe in tests.
            let status = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "umask \"$1\"; shift; exec \"$@\"",
                    "identity-umask",
                    mask,
                ])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--include-ignored",
                    "--exact",
                    "deploy::identity::tests::restrictive_umask_preserves_new_object_permissions",
                ])
                .env(CHILD, &scratch.0)
                .status()
                .unwrap();
            assert!(status.success(), "umask {mask}");
        }
    }

    #[test]
    #[ignore = "run by identity_contract_in_trusted_namespace"]
    fn unsafe_state_and_key_files_are_refused_without_modifying_targets() {
        assert!(provision(Path::new("/td/store/identity"))
            .unwrap_err()
            .contains("outside /td/store"));
        let scratch = Scratch::new();
        let state = scratch.0.join("state");
        provision(&state).unwrap();
        let key = fs::read(state.join(KEY)).unwrap();
        fs::set_permissions(state.join(KEY), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(provision(&state).is_err());
        fs::set_permissions(state.join(KEY), fs::Permissions::from_mode(0o1600)).unwrap();
        assert!(provision(&state).is_err());
        fs::set_permissions(state.join(KEY), fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(state.join(KEY), scratch.0.join("saved")).unwrap();
        symlink(scratch.0.join("saved"), state.join(KEY)).unwrap();
        assert!(provision(&state).is_err());
        assert_eq!(fs::read(scratch.0.join("saved")).unwrap(), key);
        fs::remove_file(state.join(KEY)).unwrap();
        fs::hard_link(scratch.0.join("saved"), state.join(KEY)).unwrap();
        assert!(provision(&state).is_err());
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(provision(&state).is_err());
        symlink(&state, scratch.0.join("link")).unwrap();
        assert!(provision(&scratch.0.join("link")).is_err());
    }
}
