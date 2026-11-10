//! Fixed stock-session admission; EXTERNAL claims never select a role.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

// Mirrored by engine::permissions and checked by the image recipe.
pub(crate) const UID: u32 = 992;
pub(crate) const HUMAN_UID: u32 = 1000;
pub(crate) const SOCKET: &str = "/run/td-bus/1000/bus";

pub(crate) fn check(uid: u32) -> Result<(), String> {
    if uid != UID {
        return Err(format!("the stock session broker requires uid {UID}"));
    }
    let parent = Path::new(SOCKET)
        .parent()
        .ok_or("the session bus path has no parent")?;
    // The socket constant supplies the whole chain; no parallel path list can
    // drift away from the endpoint the listener actually binds.
    for (index, path) in parent.ancestors().enumerate() {
        let owner = if index == 0 { UID } else { 0 };
        directory(path, (owner, owner), index < 2)?;
    }
    Ok(())
}

fn directory(path: &Path, owner: (u32, u32), exact: bool) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect session bus directory {}: {error}", path.display()))?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir()
        || metadata.uid() != owner.0
        || metadata.gid() != owner.1
        || mode & 0o7022 != 0
        || mode & 0o001 == 0
        || (exact && mode != 0o755)
    {
        return Err(format!(
            "{} must be an unredirected traversable directory owned by uid {} gid {} without other writers{}; got uid {} gid {} mode {mode:04o}",
            path.display(),
            owner.0,
            owner.1,
            if exact { " and with mode 0755" } else { "" },
            metadata.uid(),
            metadata.gid()
        ));
    }
    Ok(())
}

/// The shared immutable policy reader also serves jail, display and audio.
pub(crate) fn load_policy() -> Result<crate::app_policy::Policy, String> {
    let policy = crate::app_policy::load_session()?;
    if policy.owner() != HUMAN_UID {
        return Err("bus application policy names another session".into());
    }
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn shared_directory_refuses_redirects_and_other_writers() {
        assert!(check(HUMAN_UID).is_err());
        let root = std::env::temp_dir().join(format!("td-bus-directory-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let uid = fs::metadata(&root).unwrap().uid();
        let gid = fs::metadata(&root).unwrap().gid();
        directory(&root, (uid, gid), true).unwrap();
        assert!(directory(&root, (uid.wrapping_add(1), gid), true).is_err());
        let link = root.with_extension("link");
        symlink(&root, &link).unwrap();
        assert!(directory(&link, (uid, gid), true).is_err());
        fs::remove_file(link).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(directory(&root, (uid, gid), true).is_err());
        for mode in [0o555, 0o711] {
            fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
            directory(&root, (uid, gid), false).unwrap();
            assert!(directory(&root, (uid, gid), true).is_err());
        }
        for mode in [0o750, 0o775, 0o1755] {
            fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
            assert!(directory(&root, (uid, gid), false).is_err());
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir(root).unwrap();
    }
}

/// The application policy as `load` reads it: the image's declarations and
/// the published workload account ledger, both from root-controlled files.
#[cfg(test)]
mod policy_files {
    #![allow(clippy::unwrap_used)]
    use crate::app_policy::{load_declarations_from, load_from, Admission, MAX_BYTES, PATH};
    use crate::test_temp::TempDir;
    use crate::workload_accounts::FILE_NAME;
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    const DECLARED: &str = "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox\nmail\tpersonal\t\nweb\tservice\t\n";
    const LEDGER: &str = "td-accounts-v1\napplication\tfirefox\t65536\napplication\tmail\t65537\n";
    const GROWN: &str = "td-accounts-v1\napplication\tfirefox\t65536\napplication\tmail\t65537\napplication\tweb\t65540\n";

    struct Fixture {
        _root: TempDir,
        etc: PathBuf,
        state: PathBuf,
        owner: (u32, u32),
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new("td-bus-policy").unwrap();
            let etc = root.path().join("etc");
            let state = root.path().join("state");
            for directory in [root.path(), &etc, &state] {
                fs::create_dir_all(directory).unwrap();
                fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let metadata = fs::metadata(&etc).unwrap();
            let fixture = Self {
                owner: (metadata.uid(), metadata.gid()),
                _root: root,
                etc,
                state,
            };
            fixture.declare(DECLARED, 0o444);
            fixture.publish(LEDGER, 0o644);
            fixture
        }

        fn declarations(&self) -> PathBuf {
            self.etc.join(Path::new(PATH).file_name().unwrap())
        }

        fn declare(&self, text: &str, mode: u32) {
            write(&self.declarations(), text, mode);
        }

        /// Publish as firstboot does: a staged file renamed into place.
        fn publish(&self, text: &str, mode: u32) {
            let staged = self.state.join(".accounts.next");
            write(&staged, text, mode);
            fs::rename(staged, self.state.join(FILE_NAME)).unwrap();
        }

        fn load(&self, admission: Admission) -> Result<crate::app_policy::Policy, String> {
            load_from(&self.etc, &self.state, self.owner, admission)
        }
    }

    fn write(path: &Path, text: &str, mode: u32) {
        let _ = fs::remove_file(path);
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn the_declarations_come_only_from_one_root_controlled_canonical_file() {
        let fixture = Fixture::new();
        let (etc, owner) = (&fixture.etc, fixture.owner);
        assert_eq!(
            load_declarations_from(etc, owner).unwrap().to_tsv(),
            DECLARED
        );
        assert!(load_declarations_from(etc, (owner.0.wrapping_add(1), owner.1)).is_err());
        for mode in [0o644, 0o400, 0o1444] {
            fixture.declare(DECLARED, mode);
            assert!(load_declarations_from(etc, owner).is_err(), "{mode:o}");
        }
        fixture.declare(DECLARED, 0o444);
        let path = fixture.declarations();
        let other = etc.join("other");
        fs::hard_link(&path, &other).unwrap();
        assert!(load_declarations_from(etc, owner).is_err());
        fs::remove_file(&path).unwrap();
        symlink(&other, &path).unwrap();
        assert!(load_declarations_from(etc, owner).is_err());
        fs::remove_file(&path).unwrap();
        fs::remove_file(&other).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(load_declarations_from(etc, owner).is_err());
        fs::remove_dir(&path).unwrap();
        for content in [
            DECLARED.replace("1000", "01000"),
            DECLARED.replace("mail\tpersonal", "mail\t1000"),
            "x".repeat(MAX_BYTES + 1),
        ] {
            fixture.declare(&content, 0o444);
            assert!(load_declarations_from(etc, owner).is_err());
        }
        fixture.declare(DECLARED, 0o444);
        fs::set_permissions(etc, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(load_declarations_from(etc, owner).is_err());
        fs::set_permissions(etc, fs::Permissions::from_mode(0o755)).unwrap();
        let link = etc.with_extension("link");
        symlink(etc, &link).unwrap();
        assert!(load_declarations_from(&link, owner).is_err());
        fs::remove_file(link).unwrap();
        // Another session's declarations are not this broker's, whatever
        // the admission.
        fixture.declare(&DECLARED.replace("1000", "1001"), 0o444);
        assert!(fixture.load(Admission::Strict).is_err());
        assert!(fixture.load(Admission::Session).is_err());
    }

    /// A strict policy needs the published ledger, and takes uids from it.
    #[test]
    fn a_strict_policy_requires_the_published_ledger() {
        let fixture = Fixture::new();
        let policy = fixture.load(Admission::Strict).unwrap();
        assert_eq!(policy.for_name("firefox").unwrap().uid, 65536);
        assert_eq!(policy.for_name("mail").unwrap().uid, 1000);
        assert_eq!(policy.account("mail"), Some(65537));
        for mode in [0o600, 0o664, 0o444] {
            fixture.publish(LEDGER, mode);
            assert!(fixture.load(Admission::Strict).is_err(), "{mode:o}");
        }
        fs::remove_file(fixture.state.join(FILE_NAME)).unwrap();
        assert!(fixture.load(Admission::Strict).is_err());
        fixture.publish(&LEDGER.replace("65537", "065537"), 0o644);
        assert!(fixture.load(Admission::Strict).is_err());
    }

    /// A session service starts on a ledger it cannot read with no
    /// application rows: the human and personal applications keep working,
    /// every service application is refused, and the repaired ledger is
    /// read on the next miss once it changes.
    #[test]
    fn a_session_policy_starts_without_a_readable_ledger() {
        let fixture = Fixture::new();
        let ledger = fixture.state.join(FILE_NAME);
        fs::remove_file(&ledger).unwrap();
        let missing = fixture.load(Admission::Session).unwrap();
        assert!(missing.admits(1000));
        assert_eq!(missing.for_name("mail").unwrap().uid, 1000);
        assert!(missing.for_name("firefox").is_none());
        assert!(missing.for_uid(65536).is_none());
        assert!(!missing.admits(65536));
        assert_eq!(missing.ledger_reads(), 0, "an absent ledger is not read");
        fixture.publish(LEDGER, 0o644);
        assert_eq!(missing.for_uid(65536).unwrap().application, "firefox");
        // A wrong-mode ledger likewise, until a repair changes the file.
        fixture.publish(LEDGER, 0o666);
        let hostile = fixture.load(Admission::Session).unwrap();
        assert!(hostile.for_name("firefox").is_none());
        assert!(hostile.for_name("firefox").is_none());
        assert_eq!(
            hostile.ledger_reads(),
            0,
            "an unchanged bad ledger is not read again"
        );
        fixture.publish(LEDGER, 0o644);
        assert_eq!(hostile.for_name("firefox").unwrap().uid, 65536);
        assert_eq!(hostile.ledger_reads(), 1);
    }

    /// A row published after the policy loaded is found by the next miss,
    /// however soon after an earlier one, and only a ledger that extends the
    /// copy is adopted.
    #[test]
    fn a_miss_rereads_a_changed_ledger_and_keeps_only_an_extension() {
        let fixture = Fixture::new();
        let policy = fixture.load(Admission::Strict).unwrap();
        assert!(policy.for_name("web").is_none());
        assert!(policy.for_uid(65540).is_none());
        // Nothing changed, so neither miss read the ledger.
        assert_eq!(policy.ledger_reads(), 0);
        // Published right after those misses, the row is seen at once.
        fixture.publish(GROWN, 0o644);
        assert_eq!(policy.for_uid(65540).unwrap().application, "web");
        assert_eq!(policy.for_name("web").unwrap().uid, 65540);
        assert!(policy.admits(65540));
        assert_eq!(policy.ledger_reads(), 1);
        // The ledger just read is the one now published: a miss reads
        // nothing more.
        assert!(policy.for_uid(65541).is_none());
        assert_eq!(policy.ledger_reads(), 1);
        // A ledger that moved a row is not adopted.
        let held = fixture.load(Admission::Strict).unwrap();
        fixture.publish(&GROWN.replace("firefox\t65536", "firefox\t65541"), 0o644);
        assert!(held.for_uid(65541).is_none());
        assert_eq!(held.ledger_reads(), 1);
        assert_eq!(held.for_name("firefox").unwrap().uid, 65536);
    }

    /// A failed re-read is retried only once the file changes again, and
    /// neither a uid outside the application range nor an undeclared name
    /// causes a read.
    #[test]
    fn a_failed_reread_waits_for_a_change_and_other_lookups_read_nothing() {
        let fixture = Fixture::new();
        let policy = fixture.load(Admission::Strict).unwrap();
        fixture.publish(GROWN, 0o666);
        assert!(policy.for_uid(65540).is_none());
        assert_eq!(policy.ledger_reads(), 1);
        assert!(policy.for_uid(65540).is_none());
        assert!(policy.for_name("web").is_none());
        assert_eq!(policy.ledger_reads(), 1);
        fixture.publish(GROWN, 0o644);
        for uid in [98304, 131071, 2000, 65535] {
            assert!(policy.for_uid(uid).is_none(), "{uid}");
            assert!(!policy.admits(uid), "{uid}");
        }
        assert!(policy.for_name("zz").is_none());
        assert!(policy.account("zz").is_none());
        assert_eq!(policy.ledger_reads(), 1);
        assert_eq!(policy.for_uid(65540).unwrap().application, "web");
        assert_eq!(policy.ledger_reads(), 2);
    }
}
