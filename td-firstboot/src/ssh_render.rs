//! The SSH policy's publication and the boot's cutover record
//! (td-login/TOKEN-LOGIN.md, "SSH" and increment 5's A2). The record's
//! bytes, writers and reader are td-authd/DESIGN.md amendment 7's.
//!
//! Both verbs publish through `publish`: a fixed-name temporary beside
//! the target, created exclusively without following a link, written,
//! synced and renamed into place, so `sshd` never reads a partial policy
//! and td-authd never reads a partial record.

use crate::principals::{O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};
use crate::{login_state, principals, ssh_policy};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The policy `sshd` requires (`-f`), relative to the root rendered under.
pub(crate) const POLICY: &str = "run/td-sshd.conf";
/// The volatile record naming the state the console and SSH were last
/// brought to, relative to the root.
pub(crate) const RECORD: &str = "run/td-login-cutover";
/// The running kernel's boot ID, whatever root is rendered under.
pub(crate) const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";
const RECORD_VERSION: &str = "td-login-cutover-v1";

/// The reduced login state a form serves, which the record and
/// `render-ssh-policy`'s output name.
pub(crate) fn word(form: ssh_policy::Form) -> &'static str {
    match form {
        ssh_policy::Form::Ordinary => "unenrolled",
        ssh_policy::Form::Enforced => "enforced",
    }
}

/// Only a verifiably unenrolled machine gets the ordinary form; the
/// predicate reports every failure to read as unavailable, never unenrolled.
pub(crate) fn ssh_form(state: login_state::State) -> ssh_policy::Form {
    match state {
        login_state::State::Unenrolled => ssh_policy::Form::Ordinary,
        login_state::State::Enrolled | login_state::State::Unavailable(_) => {
            ssh_policy::Form::Enforced
        }
    }
}

/// The form the login state under `root` selects.
pub(crate) fn selected(root: &Path, owner: login_state::Owner) -> ssh_policy::Form {
    ssh_form(login_state::state_as(
        root,
        owner,
        principals::primary_account::UID,
    ))
}

/// A validated boot ID: 36 bytes of lowercase hex with hyphens where a
/// UUID's text form has them.
struct BootId(String);

impl BootId {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let shaped = bytes.len() == 36
            && bytes.iter().enumerate().all(|(at, byte)| {
                if matches!(at, 8 | 13 | 18 | 23) {
                    *byte == b'-'
                } else {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)
                }
            });
        match std::str::from_utf8(bytes) {
            Ok(text) if shaped => Ok(Self(text.to_owned())),
            _ => Err("the boot ID is not 36 bytes of lowercase UUID text".into()),
        }
    }

    /// The ID `path` holds as one newline-terminated line.
    fn read(path: &Path) -> Result<Self, String> {
        let mut bytes = Vec::with_capacity(38);
        std::fs::File::open(path)
            .and_then(|file| file.take(38).read_to_end(&mut bytes))
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let line = bytes
            .strip_suffix(b"\n")
            .ok_or_else(|| format!("{} is not one newline-terminated line", path.display()))?;
        Self::parse(line)
    }
}

/// The record's exact bytes: version, boot ID and the form's word, each
/// newline-terminated.
fn record(boot: &BootId, form: ssh_policy::Form) -> String {
    format!("{RECORD_VERSION}\n{}\n{}\n", boot.0, word(form))
}

/// Stage 1's render: the policy for `primary` in the form `root`'s login
/// state selects, published under `root`, then the record naming that
/// form for the boot `boot_id` names. The ID is read and checked first,
/// so a malformed one publishes nothing.
pub(crate) fn boot_render(
    root: &Path,
    primary: &str,
    boot_id: &Path,
    owner: login_state::Owner,
) -> Result<ssh_policy::Form, String> {
    let boot = BootId::read(boot_id)?;
    let form = publish_policy(root, primary, owner)?;
    publish(&root.join(RECORD), record(&boot, form).as_bytes(), owner)?;
    Ok(form)
}

/// The running boot's render: the policy alone, and the line
/// `render-ssh-policy` prints for the form it published.
pub(crate) fn running_render(
    root: &Path,
    primary: &str,
    owner: login_state::Owner,
) -> Result<String, String> {
    publish_policy(root, primary, owner).map(|form| format!("{}\n", word(form)))
}

fn publish_policy(
    root: &Path,
    primary: &str,
    owner: login_state::Owner,
) -> Result<ssh_policy::Form, String> {
    let form = selected(root, owner);
    publish(
        &root.join(POLICY),
        ssh_policy::config(primary, form).as_bytes(),
        owner,
    )?;
    Ok(form)
}

/// The fixed temporary beside `path`.
fn temporary(path: &Path) -> PathBuf {
    beside(path, ".tmp")
}

fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Opens, creating it if absent, the persistent lock file beside `path`
/// and takes its exclusive lock, which the kernel drops if the process
/// dies. It must be a single-link regular file of `owner`, mode 0600, so
/// only that owner can hold it: an account that could open the lock could
/// stall every render.
fn lock(path: &Path, owner: login_state::Owner) -> std::io::Result<std::fs::File> {
    let lock = beside(path, ".lock");
    let named = |error: std::io::Error| {
        std::io::Error::new(error.kind(), format!("{}: {error}", lock.display()))
    };
    let open = |create: bool| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(create)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(&lock)
    };
    // A new file takes the owner and mode here, so neither a setgid parent nor
    // the umask can leave one the check below refuses forever; an existing one
    // is checked, never repaired.
    let file = match open(true) {
        Ok(file) => {
            std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid)).map_err(named)?;
            file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .map_err(named)?;
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            open(false).map_err(named)?
        }
        Err(error) => return Err(named(error)),
    };
    let meta = file.metadata().map_err(named)?;
    if !meta.file_type().is_file()
        || (meta.uid(), meta.gid()) != (owner.uid, owner.gid)
        || meta.mode() & 0o7777 != 0o600
        || meta.nlink() != 1
    {
        return Err(std::io::Error::other(format!(
            "{} is not a single-link {}:{} mode-0600 regular file",
            lock.display(),
            owner.uid,
            owner.gid
        )));
    }
    file.lock().map_err(named)?;
    Ok(file)
}

/// Publishes `bytes` at `path`, `owner`'s and mode 0600, through the
/// fixed temporary: a stale one is unlinked, never followed, and the new
/// one is created with `O_CREAT|O_EXCL|O_NOFOLLOW`. Owner and mode are set
/// through its descriptor before the file and then its directory are
/// synced around the rename, which replaces whatever entry `path` names
/// without following it. The whole sequence holds the lock beside `path`,
/// so an overlapping publisher cannot unlink this one's temporary or
/// rename it half written.
fn publish(path: &Path, bytes: &[u8], owner: login_state::Owner) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    let temporary = temporary(path);
    let write = || -> std::io::Result<()> {
        let parent = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(directory)?;
        let _held = lock(path, owner)?;
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = create(&temporary)?;
        std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid))?;
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        parent.sync_all()
    };
    write().map_err(|error| {
        format!(
            "publish {} through {}: {error}",
            path.display(),
            temporary.display()
        )
    })
}

fn create(temporary: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(temporary)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    const ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    /// A root holding `run` and a valid `var/lib/td/login`, owned by the
    /// test's IDs, and a boot ID file beside them.
    struct Root {
        root: PathBuf,
        owner: login_state::Owner,
    }

    impl Root {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "td-firstboot-ssh-render-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&root);
            for directory in ["var/lib/td", "run"] {
                std::fs::DirBuilder::new()
                    .mode(0o755)
                    .recursive(true)
                    .create(root.join(directory))
                    .unwrap();
            }
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(login_state::directory(&root))
                .unwrap();
            let meta = std::fs::metadata(&root).unwrap();
            let root = Self {
                root,
                owner: login_state::Owner {
                    uid: meta.uid(),
                    gid: meta.gid(),
                },
            };
            root.boot_id(format!("{ID}\n").as_bytes());
            root
        }

        fn boot_id(&self, bytes: &[u8]) {
            std::fs::write(self.root.join("boot_id"), bytes).unwrap();
        }

        fn boot(&self) -> Result<ssh_policy::Form, String> {
            boot_render(&self.root, "alice", &self.root.join("boot_id"), self.owner)
        }

        fn running(&self) -> Result<String, String> {
            running_render(&self.root, "alice", self.owner)
        }

        fn enroll(&self) {
            std::fs::write(login_state::directory(&self.root).join("1000"), b"").unwrap();
        }

        fn read(&self, name: &str) -> Vec<u8> {
            std::fs::read(self.root.join(name)).unwrap()
        }

        /// Mode 0600, the test's owner, a regular file, and no temporary.
        fn published(&self, name: &str) {
            let meta = std::fs::symlink_metadata(self.root.join(name)).unwrap();
            assert!(meta.file_type().is_file(), "{name}");
            assert_eq!(meta.mode() & 0o7777, 0o600, "{name}");
            assert_eq!((meta.uid(), meta.gid()), (self.owner.uid, self.owner.gid));
            assert_eq!(meta.nlink(), 1);
            assert!(std::fs::symlink_metadata(temporary(&self.root.join(name))).is_err());
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// td-authd/DESIGN.md amendment 7's three lines, written out.
    #[test]
    fn the_record_is_the_amendments_exact_bytes_for_both_forms() {
        let root = Root::new();
        assert_eq!(root.boot(), Ok(ssh_policy::Form::Ordinary));
        assert_eq!(
            root.read(RECORD),
            b"td-login-cutover-v1\n0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\nunenrolled\n"
        );
        root.published(RECORD);
        root.published(POLICY);
        assert_eq!(
            root.read(POLICY),
            ssh_policy::config("alice", ssh_policy::Form::Ordinary).as_bytes()
        );
        root.enroll();
        assert_eq!(root.boot(), Ok(ssh_policy::Form::Enforced));
        assert_eq!(
            root.read(RECORD),
            b"td-login-cutover-v1\n0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\nenforced\n"
        );
        assert_eq!(
            root.read(POLICY),
            ssh_policy::config("alice", ssh_policy::Form::Enforced).as_bytes()
        );
        root.published(RECORD);
        root.published(POLICY);
        assert_eq!(RECORD, "run/td-login-cutover");
        assert_eq!(POLICY, "run/td-sshd.conf");
        assert_eq!(BOOT_ID, "/proc/sys/kernel/random/boot_id");
    }

    /// The in-boot verb prints exactly the word of the form it published
    /// and writes no record; for one state both verbs publish one policy.
    #[test]
    fn the_running_render_reports_its_form_and_matches_the_boot_render() {
        let root = Root::new();
        assert_eq!(root.running().as_deref(), Ok("unenrolled\n"));
        assert!(std::fs::symlink_metadata(root.root.join(RECORD)).is_err());
        root.published(POLICY);
        let running = root.read(POLICY);
        root.boot().unwrap();
        assert_eq!(root.read(POLICY), running);
        root.enroll();
        assert_eq!(root.running().as_deref(), Ok("enforced\n"));
        let running = root.read(POLICY);
        assert_eq!(
            running,
            ssh_policy::config("alice", ssh_policy::Form::Enforced).as_bytes()
        );
        // The record still names what stage 1 rendered.
        assert!(String::from_utf8(root.read(RECORD))
            .unwrap()
            .ends_with("\nunenrolled\n"));
        root.boot().unwrap();
        assert_eq!(root.read(POLICY), running);
        // A damaged directory reads as unavailable: enforced, both ways.
        let login = login_state::directory(&root.root);
        std::fs::remove_file(login.join("1000")).unwrap();
        std::fs::set_permissions(&login, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(root.running().as_deref(), Ok("enforced\n"));
        assert_eq!(root.boot(), Ok(ssh_policy::Form::Enforced));
        assert_eq!(root.read(POLICY), running);
    }

    #[test]
    fn a_stale_temporary_is_replaced_and_a_link_is_never_followed() {
        let root = Root::new();
        let outside = root.root.join("outside");
        for name in [POLICY, RECORD] {
            let target = root.root.join(name);
            let temporary = temporary(&target);
            // A stale regular temporary, with bytes the result must not keep.
            std::fs::write(&temporary, b"stale stale stale stale stale").unwrap();
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644)).unwrap();
            root.boot().unwrap();
            root.published(name);
            // A link at the temporary is unlinked, not written through.
            std::os::unix::fs::symlink(&outside, &temporary).unwrap();
            root.boot().unwrap();
            assert!(!outside.exists());
            root.published(name);
            // A link at the target is replaced by the rename, not followed.
            std::fs::write(&outside, b"outside").unwrap();
            std::fs::remove_file(&target).unwrap();
            std::os::unix::fs::symlink(&outside, &target).unwrap();
            root.boot().unwrap();
            assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
            root.published(name);
            std::fs::remove_file(&outside).unwrap();
        }
        // The exclusive creation itself refuses a link, dangling or not.
        let temporary = root.root.join("run/raced.tmp");
        std::os::unix::fs::symlink(&outside, &temporary).unwrap();
        assert!(create(&temporary).is_err());
        assert!(!outside.exists());
        std::fs::write(&outside, b"outside").unwrap();
        assert!(create(&temporary).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
        // A directory at the target fails the publication.
        let blocked = root.root.join("run/blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("kept"), b"").unwrap();
        assert!(publish(&blocked, b"x", root.owner).is_err());
        assert!(blocked.join("kept").exists());
    }

    /// A publisher waits for the lock beside its target, so two never
    /// interleave; the lock is a root-only file, not the shared directory.
    #[test]
    fn a_publisher_waits_for_the_lock_beside_its_target() {
        let root = Root::new();
        let target = root.root.join(POLICY);
        let owner = root.owner;
        let held = lock(&target, owner).unwrap();
        let publisher = {
            let target = target.clone();
            std::thread::spawn(move || publish(&target, b"policy", owner))
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!publisher.is_finished());
        assert!(std::fs::symlink_metadata(&target).is_err());
        drop(held);
        assert_eq!(publisher.join().unwrap(), Ok(()));
        assert_eq!(std::fs::read(&target).unwrap(), b"policy");
        root.published(POLICY);
        // The lock persists, owner-only, and is reused.
        let lock_path = beside(&target, ".lock");
        let meta = std::fs::symlink_metadata(&lock_path).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.mode() & 0o7777, 0o600);
        assert_eq!((meta.uid(), meta.gid()), (owner.uid, owner.gid));
        publish(&target, b"again", owner).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"again");
        // A shared directory lock does not stall a publisher.
        let run = std::fs::File::open(root.root.join("run")).unwrap();
        run.lock().unwrap();
        publish(&target, b"unblocked", owner).unwrap();
        drop(run);
        // A link standing for the directory is refused, not locked through.
        let elsewhere = root.root.join("elsewhere");
        std::os::unix::fs::symlink(root.root.join("run"), &elsewhere).unwrap();
        assert!(publish(&elsewhere.join("td-sshd.conf"), b"x", owner).is_err());
    }

    /// A lock file anyone else could hold is refused, never repaired.
    #[test]
    fn a_lock_that_is_not_the_owners_alone_is_refused() {
        let root = Root::new();
        let target = root.root.join(POLICY);
        let lock_path = beside(&target, ".lock");
        let refused = |owner: login_state::Owner| {
            let error = publish(&target, b"x", owner).unwrap_err();
            assert!(error.contains(".lock"), "{error}");
            assert!(std::fs::symlink_metadata(&target).is_err());
        };
        std::fs::write(&lock_path, b"").unwrap();
        for mode in [0o644, 0o666, 0o400, 0o4600] {
            std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(mode)).unwrap();
            refused(root.owner);
            assert_eq!(std::fs::metadata(&lock_path).unwrap().mode() & 0o7777, mode);
        }
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        refused(login_state::Owner {
            uid: root.owner.uid ^ 1,
            ..root.owner
        });
        refused(login_state::Owner {
            gid: root.owner.gid ^ 1,
            ..root.owner
        });
        let second = root.root.join("run/second-link");
        std::fs::hard_link(&lock_path, &second).unwrap();
        refused(root.owner);
        std::fs::remove_file(&second).unwrap();
        // A link at the lock is not followed, and creates nothing outside.
        let outside = root.root.join("outside");
        std::fs::remove_file(&lock_path).unwrap();
        std::os::unix::fs::symlink(&outside, &lock_path).unwrap();
        assert!(publish(&target, b"x", root.owner).is_err());
        assert!(!outside.exists());
        std::fs::remove_file(&lock_path).unwrap();
        std::fs::create_dir(&lock_path).unwrap();
        assert!(publish(&target, b"x", root.owner).is_err());
        std::fs::remove_dir(&lock_path).unwrap();
        publish(&target, b"x", root.owner).unwrap();
        root.published(POLICY);
    }

    /// A malformed boot ID fails the boot render before anything is
    /// published.
    #[test]
    fn a_malformed_boot_id_is_refused() {
        let root = Root::new();
        let upper = ID.to_uppercase();
        let shifted = format!("{}-{}{}", &ID[..7], &ID[7..8], &ID[9..]);
        let long = format!("{ID}0");
        let bad: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"\n".to_vec(),
            ID.as_bytes().to_vec(),
            format!("{ID}\n\n").into_bytes(),
            format!("{upper}\n").into_bytes(),
            format!("{long}\n").into_bytes(),
            format!("{}\n", &ID[..35]).into_bytes(),
            format!("{shifted}\n").into_bytes(),
            format!("{}\n", ID.replace('-', "0")).into_bytes(),
            format!("{}g\n", &ID[..35]).into_bytes(),
            format!("{}\u{e9}\n", &ID[..34]).into_bytes(),
            format!(" {}\n", &ID[1..]).into_bytes(),
        ];
        for bytes in &bad {
            root.boot_id(bytes);
            assert!(root.boot().is_err(), "{:?}", String::from_utf8_lossy(bytes));
            assert!(std::fs::symlink_metadata(root.root.join(POLICY)).is_err());
            assert!(std::fs::symlink_metadata(root.root.join(RECORD)).is_err());
        }
        assert!(BootId::read(&root.root.join("absent")).is_err());
        root.boot_id(format!("{ID}\n").as_bytes());
        assert!(root.boot().is_ok());
    }

    /// The record cannot be published, so the render fails; the policy it
    /// published first is complete.
    #[test]
    fn a_record_that_cannot_be_published_fails_the_boot_render() {
        let root = Root::new();
        std::fs::create_dir(root.root.join(RECORD)).unwrap();
        std::fs::write(root.root.join(RECORD).join("kept"), b"").unwrap();
        assert!(root.boot().is_err());
        root.published(POLICY);
        // An absent directory fails the policy itself.
        let bare = Root::new();
        std::fs::remove_dir(bare.root.join("run")).unwrap();
        assert!(bare.boot().is_err());
        assert!(bare.running().is_err());
    }

    /// The live kernel's ID, where the host has one, is the shape the record
    /// requires.
    #[test]
    fn the_running_kernels_boot_id_is_admitted() {
        if Path::new(BOOT_ID).exists() {
            assert!(BootId::read(Path::new(BOOT_ID)).is_ok());
        }
    }
}
