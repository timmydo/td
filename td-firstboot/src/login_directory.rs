//! The login directory at every boot (td-login/TOKEN-LOGIN.md, "The login
//! record"): create what is absent, accept what is valid, refuse rather than
//! repair anything else, and remove only the record store's temporaries.

use crate::login_state::{self, Directory, Owner};
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{fchown, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `/var/lib/td`, beside the persistent state td-firstboot keeps there.
const STATE_MODE: u32 = 0o755;
const LOGIN_MODE: u32 = 0o700;

/// Ensures `<root>/var/lib/td/login`: each of `td` and `login` is created,
/// owned by root and given its mode only when absent and only in a parent
/// that root owns and no one else may write, and nothing that
/// exists is chowned or chmodded. A valid directory then loses its `tmp-`
/// entries and nothing else. The error is one line naming what was refused.
pub(crate) fn ensure(root: &Path) -> Result<(), String> {
    ensure_as(Path::new(login_state::FD_ROOT), root, Owner::ROOT)
}

/// Whether `<root>/var/lib/td/login` is valid, without writing anything.
pub(crate) fn check(root: &Path) -> Result<(), String> {
    Directory::open(&login_state::directory(root), Owner::ROOT)
        .map(drop)
        .map_err(|refusal| refusal.reason)
}

fn ensure_as(fd_root: &Path, root: &Path, owner: Owner) -> Result<(), String> {
    ensure_with(fd_root, root, owner, &mut |_| ())
}

/// `ensure_as`, with `created` called after each `mkdir` and before the
/// directory is opened, where tests stand in for another process.
fn ensure_with(
    fd_root: &Path,
    root: &Path,
    owner: Owner,
    created: &mut dyn FnMut(&Path),
) -> Result<(), String> {
    let login = login_state::directory(root);
    let (Some(state), Some(lib)) = (login.parent(), login.parent().and_then(Path::parent)) else {
        return Err(format!("{login:?} has no parent"));
    };
    let lib_file = login_state::walk(fd_root, lib).map_err(|refusal| refusal.reason)?;
    let parent = Parent {
        fd_root,
        file: &lib_file,
        path: lib,
    };
    let state_file = parent.child(state, STATE_MODE, owner, created)?;
    let parent = Parent {
        fd_root,
        file: &state_file,
        path: state,
    };
    parent.child(&login, LOGIN_MODE, owner, created)?;
    // The created or existing directory, walked again from `/` as every
    // reader walks it.
    let directory =
        Directory::open_via(fd_root, &login, owner).map_err(|refusal| refusal.reason)?;
    directory.remove_temporaries()
}

/// A held directory that a child is opened or created in.
struct Parent<'a> {
    fd_root: &'a Path,
    file: &'a File,
    path: &'a Path,
}

impl Parent<'_> {
    fn at(&self, file: &File, name: impl AsRef<Path>) -> PathBuf {
        self.fd_root.join(file.as_raw_fd().to_string()).join(name)
    }

    /// Only `owner`, root in production, may write in a directory firstboot
    /// creates in, so no other process can rename a directory into the name
    /// between its `mkdir` and its open.
    fn trusted(&self, owner: Owner) -> Result<(), String> {
        let path = self.path;
        let meta = self
            .file
            .metadata()
            .map_err(|e| format!("inspect {path:?}: {e}"))?;
        if meta.uid() != owner.uid {
            return Err(format!(
                "{path:?} is owned by UID {}, not {}, so nothing is created in it",
                meta.uid(),
                owner.uid
            ));
        }
        let mode = meta.mode() & 0o7777;
        if mode & 0o022 != 0 {
            return Err(format!(
                "{path:?} has mode {mode:04o}, writable by others, so nothing is created in it"
            ));
        }
        Ok(())
    }

    /// The directory `path` names in this parent: an existing one opened
    /// without following a link and left exactly as it is; an absent one,
    /// in a trusted parent only, which is what keeps another process from
    /// substituting it, created with `mode` and handed to `owner` through
    /// its own descriptor once that descriptor shows an empty directory
    /// (defence in depth).
    fn child(
        &self,
        path: &Path,
        mode: u32,
        owner: Owner,
        created: &mut dyn FnMut(&Path),
    ) -> Result<File, String> {
        let Some(name) = path.file_name() else {
            return Err(format!("{path:?} has no name"));
        };
        let at = self.at(self.file, name);
        let open = || {
            OpenOptions::new()
                .read(true)
                .custom_flags(login_state::OPEN_DIRECTORY | login_state::NOFOLLOW)
                .open(&at)
        };
        let refused = |error: io::Error| login_state::refuse_open(&error, &at, path).reason;
        match open() {
            Ok(existing) => return Ok(existing),
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(refused(error)),
            Err(_) => {}
        }
        self.trusted(owner)?;
        match DirBuilder::new().mode(mode).create(&at) {
            Ok(()) => {}
            // Made meanwhile by a writer the trusted parent admits: existing.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return open().map_err(refused);
            }
            Err(error) => return Err(format!("create {path:?}: {error}")),
        }
        created(&at);
        let made = open().map_err(|e| format!("open created {path:?}: {e}"))?;
        let links = made
            .metadata()
            .map_err(|e| format!("inspect created {path:?}: {e}"))?
            .nlink();
        let empty = fs::read_dir(self.at(&made, ""))
            .map_err(|e| format!("list created {path:?}: {e}"))?
            .next()
            .is_none();
        if !untouched(empty, links) {
            return Err(format!(
                "created {path:?} is not the empty directory its mkdir made, so it is left as it is"
            ));
        }
        fchown(&made, Some(owner.uid), Some(owner.gid))
            .map_err(|e| format!("chown created {path:?}: {e}"))?;
        // The umask may have narrowed the creation mode.
        made.set_permissions(Permissions::from_mode(mode))
            .map_err(|e| format!("chmod created {path:?}: {e}"))?;
        made.sync_all()
            .and_then(|()| self.file.sync_all())
            .map_err(|e| format!("sync created {path:?}: {e}"))?;
        Ok(made)
    }
}

/// A directory just made: no entries, and two links, or the one Btrfs
/// reports for every directory.
fn untouched(empty: bool, links: u64) -> bool {
    empty && matches!(links, 1 | 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A scratch root holding `var/lib`, owned by the test's own IDs, which
    /// stand for root's: a test cannot create a directory root owns.
    struct Root {
        root: PathBuf,
        owner: Owner,
    }

    impl Root {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "td-firstboot-login-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new()
                .mode(0o755)
                .recursive(true)
                .create(root.join("var/lib"))
                .unwrap();
            // Whatever the umask, only the owner writes in the parents.
            for path in [root.join("var/lib"), root.join("var"), root.clone()] {
                fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
            }
            let meta = fs::metadata(&root).unwrap();
            Self {
                root,
                owner: Owner {
                    uid: meta.uid(),
                    gid: meta.gid(),
                },
            }
        }

        fn state(&self) -> PathBuf {
            self.root.join("var/lib/td")
        }

        fn login(&self) -> PathBuf {
            self.root.join("var/lib/td/login")
        }

        fn ensure(&self) -> Result<(), String> {
            ensure_as(Path::new(login_state::FD_ROOT), &self.root, self.owner)
        }

        fn ensure_as(&self, owner: Owner) -> Result<(), String> {
            ensure_as(Path::new(login_state::FD_ROOT), &self.root, owner)
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.login())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.login(), Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Everything a chmod, chown or rewrite of `path` would move.
    fn stamp(path: &Path) -> (u64, u32, u32, u32, i64, i64, i64, i64) {
        let meta = fs::symlink_metadata(path).unwrap();
        (
            meta.ino(),
            meta.mode(),
            meta.uid(),
            meta.gid(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn an_absent_directory_is_created_root_style_and_then_accepted() {
        let root = Root::new();
        root.ensure().unwrap();
        assert_eq!(mode(&root.state()), 0o755);
        assert_eq!(mode(&root.login()), 0o700);
        for path in [root.state(), root.login()] {
            let meta = fs::symlink_metadata(&path).unwrap();
            assert!(meta.is_dir());
            assert_eq!((meta.uid(), meta.gid()), (root.owner.uid, root.owner.gid));
        }
        assert!(root.names().is_empty());
        assert_eq!(
            login_state::state_as(&root.root, root.owner, 1000),
            login_state::State::Unenrolled
        );
        // A second boot, and a third, change nothing.
        let before = (stamp(&root.state()), stamp(&root.login()));
        root.ensure().unwrap();
        root.ensure().unwrap();
        assert_eq!((stamp(&root.state()), stamp(&root.login())), before);
    }

    #[test]
    fn an_existing_state_directory_is_used_as_it_is() {
        let root = Root::new();
        // Not the mode firstboot would create, but nothing it may change.
        fs::DirBuilder::new()
            .mode(0o711)
            .create(root.state())
            .unwrap();
        fs::set_permissions(root.state(), Permissions::from_mode(0o711)).unwrap();
        fs::write(root.state().join("machine-id"), b"kept").unwrap();
        let before = stamp(&root.state());
        root.ensure().unwrap();
        // Creating `login` moves the parent's mtime, and nothing else.
        let after = stamp(&root.state());
        assert_eq!(
            (after.0, after.1, after.2, after.3),
            (before.0, before.1, before.2, before.3)
        );
        assert_eq!(mode(&root.state()), 0o711);
        assert_eq!(mode(&root.login()), 0o700);
        assert_eq!(fs::read(root.state().join("machine-id")).unwrap(), b"kept");
    }

    #[test]
    fn an_existing_valid_directory_is_never_chmodded_or_chowned() {
        let root = Root::new();
        root.ensure().unwrap();
        fs::write(root.login().join("1000"), b"record").unwrap();
        let before = (
            stamp(&root.state()),
            stamp(&root.login()),
            stamp(&root.login().join("1000")),
        );
        root.ensure().unwrap();
        assert_eq!(
            (
                stamp(&root.state()),
                stamp(&root.login()),
                stamp(&root.login().join("1000")),
            ),
            before
        );
    }

    #[test]
    fn invalid_metadata_is_refused_and_left_as_it_is() {
        let root = Root::new();
        let login = root.login();
        root.ensure().unwrap();
        // Any mode but 0700.
        for wrong in [0o755, 0o750, 0o701, 0o500, 0o1700] {
            fs::set_permissions(&login, Permissions::from_mode(wrong)).unwrap();
            let before = stamp(&login);
            assert_eq!(
                root.ensure(),
                Err(format!("{login:?} has mode {wrong:04o}, not 0700"))
            );
            assert_eq!(stamp(&login), before);
        }
        fs::set_permissions(&login, Permissions::from_mode(0o700)).unwrap();
        // Another owner or group than the one required.
        for other in [
            Owner {
                uid: root.owner.uid ^ 1,
                ..root.owner
            },
            Owner {
                gid: root.owner.gid ^ 1,
                ..root.owner
            },
        ] {
            let before = stamp(&login);
            let refused = root.ensure_as(other).unwrap_err();
            assert_eq!(
                refused,
                format!(
                    "{login:?} is owned by {}:{}, not {}:{}",
                    root.owner.uid, root.owner.gid, other.uid, other.gid
                )
            );
            assert_eq!(stamp(&login), before);
        }
        // A file, a link to a valid directory, a dangling link.
        fs::remove_dir(&login).unwrap();
        fs::write(&login, b"file").unwrap();
        assert_eq!(root.ensure(), Err(format!("{login:?} is not a directory")));
        assert_eq!(fs::read(&login).unwrap(), b"file");
        fs::remove_file(&login).unwrap();
        let elsewhere = root.root.join("elsewhere");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&elsewhere)
            .unwrap();
        fs::write(elsewhere.join("tmp-1"), b"kept").unwrap();
        symlink(&elsewhere, &login).unwrap();
        assert_eq!(root.ensure(), Err(format!("{login:?} is a symbolic link")));
        assert_eq!(fs::read(elsewhere.join("tmp-1")).unwrap(), b"kept");
        fs::remove_file(&login).unwrap();
        symlink("missing", &login).unwrap();
        assert_eq!(root.ensure(), Err(format!("{login:?} is a symbolic link")));
        assert!(fs::symlink_metadata(&login)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!root.state().join("missing").exists());
    }

    #[test]
    fn a_link_above_the_directory_is_refused() {
        // `td` a link to a valid tree.
        let root = Root::new();
        let real = root.root.join("var/lib/real");
        fs::DirBuilder::new().mode(0o755).create(&real).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(real.join("login"))
            .unwrap();
        symlink("real", root.state()).unwrap();
        assert_eq!(
            root.ensure(),
            Err(format!("{:?} is a symbolic link", root.state()))
        );
        assert_eq!(fs::read_dir(real.join("login")).unwrap().count(), 0);
        // `lib` a link: the walk refuses before creating anything.
        let root = Root::new();
        let lib = root.root.join("var/lib");
        fs::remove_dir(&lib).unwrap();
        let other = root.root.join("other");
        fs::DirBuilder::new().mode(0o755).create(&other).unwrap();
        symlink(&other, &lib).unwrap();
        assert_eq!(root.ensure(), Err(format!("{lib:?} is a symbolic link")));
        assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
        // `var` a link.
        let root = Root::new();
        let var = root.root.join("var");
        let moved = root.root.join("moved");
        fs::rename(&var, &moved).unwrap();
        symlink(&moved, &var).unwrap();
        assert_eq!(root.ensure(), Err(format!("{var:?} is a symbolic link")));
        assert!(!moved.join("lib/td").exists());
    }

    #[test]
    fn a_missing_parent_is_refused_and_not_created() {
        let root = Root::new();
        let lib = root.root.join("var/lib");
        fs::remove_dir(&lib).unwrap();
        assert_eq!(root.ensure(), Err(format!("{lib:?} is missing")));
        assert!(!lib.exists());
        // A file where `lib` would be.
        fs::write(&lib, b"").unwrap();
        assert_eq!(root.ensure(), Err(format!("{lib:?} is not a directory")));
        // `td` a file.
        let root = Root::new();
        fs::write(root.state(), b"").unwrap();
        assert_eq!(
            root.ensure(),
            Err(format!("{:?} is not a directory", root.state()))
        );
        // A relative root.
        assert!(ensure_as(
            Path::new(login_state::FD_ROOT),
            Path::new("relative"),
            Owner::ROOT
        )
        .unwrap_err()
        .ends_with("is not absolute"));
    }

    /// No directory is created where anyone but root (here the test's own
    /// UID) may write, or that root does not own: another writer could put
    /// its own directory under the name between the mkdir and the open.
    #[test]
    fn an_untrusted_parent_refuses_and_nothing_is_created_in_it() {
        let root = Root::new();
        let lib = root.root.join("var/lib");
        for wrong in [0o775, 0o757, 0o777, 0o1777, 0o2775] {
            fs::set_permissions(&lib, Permissions::from_mode(wrong)).unwrap();
            assert_eq!(
                root.ensure(),
                Err(format!(
                    "{lib:?} has mode {wrong:04o}, writable by others, so nothing is created in it"
                ))
            );
            assert!(!root.state().exists(), "{wrong:o}");
            assert_eq!(mode(&lib), wrong);
        }
        fs::set_permissions(&lib, Permissions::from_mode(0o755)).unwrap();
        let other = Owner {
            uid: root.owner.uid ^ 1,
            ..root.owner
        };
        assert_eq!(
            root.ensure_as(other),
            Err(format!(
                "{lib:?} is owned by UID {}, not {}, so nothing is created in it",
                root.owner.uid, other.uid
            ))
        );
        assert!(!root.state().exists());
        // The same for `login` in an existing `td`, whose mode stays.
        fs::DirBuilder::new().create(root.state()).unwrap();
        fs::set_permissions(root.state(), Permissions::from_mode(0o777)).unwrap();
        assert!(root
            .ensure()
            .unwrap_err()
            .starts_with(&format!("{:?} has mode 0777", root.state())));
        assert!(!root.login().exists());
        assert_eq!(mode(&root.state()), 0o777);
        // Trust is asked of a parent only when something is created in it.
        fs::set_permissions(root.state(), Permissions::from_mode(0o755)).unwrap();
        root.ensure().unwrap();
        fs::set_permissions(root.state(), Permissions::from_mode(0o775)).unwrap();
        root.ensure().unwrap();
        assert_eq!(mode(&root.state()), 0o775);
    }

    /// What the name holds after the mkdir must be the empty directory it
    /// made; anything else is refused and neither chowned nor chmodded.
    #[test]
    fn a_filled_or_substituted_directory_is_refused_and_left_as_it_is() {
        let ensure = |root: &Root, created: &mut dyn FnMut(&Path)| {
            ensure_with(
                Path::new(login_state::FD_ROOT),
                &root.root,
                root.owner,
                created,
            )
        };
        for (target, leaf) in [("td", false), ("login", true)] {
            let refused = |root: &Root| {
                format!(
                    "created {:?} is not the empty directory its mkdir made, so it is left as it is",
                    if leaf { root.login() } else { root.state() }
                )
            };
            // Something written into the new directory before the open.
            let root = Root::new();
            let mut filled = |at: &Path| {
                if at.ends_with(target) {
                    fs::write(at.join("planted"), b"kept").unwrap();
                    fs::set_permissions(at, Permissions::from_mode(0o751)).unwrap();
                }
            };
            assert_eq!(ensure(&root, &mut filled), Err(refused(&root)));
            let path = if leaf { root.login() } else { root.state() };
            assert_eq!(mode(&path), 0o751);
            assert_eq!(fs::read(path.join("planted")).unwrap(), b"kept");
            // Another directory renamed over the new one before the open.
            let root = Root::new();
            let decoy = root.root.join("decoy");
            fs::DirBuilder::new().create(&decoy).unwrap();
            fs::create_dir(decoy.join("sub")).unwrap();
            fs::set_permissions(&decoy, Permissions::from_mode(0o750)).unwrap();
            let mut substituted = |at: &Path| {
                if at.ends_with(target) {
                    fs::remove_dir(at).unwrap();
                    fs::rename(&decoy, at).unwrap();
                }
            };
            assert_eq!(ensure(&root, &mut substituted), Err(refused(&root)));
            let path = if leaf { root.login() } else { root.state() };
            assert_eq!(mode(&path), 0o750);
            assert!(path.join("sub").is_dir());
            if !leaf {
                assert!(!root.login().exists());
            }
        }
    }

    #[test]
    fn a_made_directory_is_empty_with_one_or_two_links() {
        assert!(untouched(true, 2));
        assert!(untouched(true, 1));
        for (empty, links) in [(false, 2), (false, 1), (true, 0), (true, 3)] {
            assert!(!untouched(empty, links), "{empty} {links}");
        }
    }

    #[test]
    fn only_temporaries_are_removed() {
        let root = Root::new();
        root.ensure().unwrap();
        let login = root.login();
        let kept = [
            ".tmp-1",
            "1000",
            "1001",
            "TMP-1",
            "cutover-reboot",
            "tmp",
            "tmp_1",
            "xtmp-1",
        ];
        for name in kept {
            fs::write(login.join(name), name).unwrap();
        }
        let victim = root.root.join("victim");
        fs::write(&victim, b"outside").unwrap();
        symlink(&victim, login.join("tmp-link")).unwrap();
        for name in ["tmp-", "tmp-0123456789abcdef0123456789abcdef", "tmp-torn"] {
            fs::write(login.join(name), b"partial").unwrap();
        }
        root.ensure().unwrap();
        assert_eq!(root.names(), kept);
        for name in kept {
            assert_eq!(fs::read(login.join(name)).unwrap(), name.as_bytes());
        }
        assert_eq!(fs::read(&victim).unwrap(), b"outside");
        // A prefixed entry that cannot be unlinked is reported by name and kept.
        fs::create_dir(login.join("tmp-dir")).unwrap();
        fs::write(login.join("tmp-dir/inside"), b"kept").unwrap();
        let refused = root.ensure().unwrap_err();
        assert!(
            refused.starts_with(&format!("unlink \"tmp-dir\" in {login:?}: ")),
            "{refused}"
        );
        assert_eq!(fs::read(login.join("tmp-dir/inside")).unwrap(), b"kept");
        // An invalid directory's temporaries are not touched.
        fs::remove_file(login.join("tmp-dir/inside")).unwrap();
        fs::remove_dir(login.join("tmp-dir")).unwrap();
        fs::write(login.join("tmp-left"), b"").unwrap();
        fs::set_permissions(&login, Permissions::from_mode(0o750)).unwrap();
        assert!(root.ensure().is_err());
        fs::set_permissions(&login, Permissions::from_mode(0o700)).unwrap();
        assert!(login.join("tmp-left").exists());
    }

    #[test]
    fn a_lost_proc_refuses_and_writes_nothing() {
        let root = Root::new();
        let lost = root.root.join("lost");
        assert!(ensure_as(&lost, &root.root, root.owner).is_err());
        assert!(!root.state().exists());
    }

    #[test]
    fn the_production_check_requires_root_ownership() {
        let root = Root::new();
        root.ensure().unwrap();
        assert_eq!(
            check(&root.root).is_ok(),
            root.owner == Owner::ROOT,
            "the check expects root:root"
        );
        assert!(check(&root.root.join("absent")).is_err());
    }
}
