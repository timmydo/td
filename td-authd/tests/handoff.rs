#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::admission::Method;
use std::fs::Permissions;
use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const UID: u32 = 1000;
const ID: &str = "00112233445566778899aabbccddeeff0123456789abcdeffedcba9876543210";

/// A temporary root holding a `run` the fake mountinfo calls tmpfs, its
/// `td-admit` directory, the deployment's ID, an uptime and the login
/// directory, all the test's own.
pub(crate) struct Machine {
    root: PathBuf,
    pub(crate) paths: Paths,
}

impl Machine {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "td-authd-handoff-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let run = root.join("run");
        fs::DirBuilder::new().mode(0o755).create(&run).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(run.join("td-admit"))
            .unwrap();
        fs::write(run.join("td-deployment"), format!("{ID}\n")).unwrap();
        fs::DirBuilder::new()
            .mode(0o755)
            .recursive(true)
            .create(root.join("var/lib/td"))
            .unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("var/lib/td/login"))
            .unwrap();
        let meta = fs::metadata(&root).unwrap();
        let machine = Self {
            paths: Paths {
                run,
                root: root.clone(),
                owner: Owner {
                    uid: meta.uid(),
                    gid: meta.gid(),
                },
                mountinfo: root.join("mountinfo"),
                uptime: root.join("uptime"),
            },
            root,
        };
        machine.mount("tmpfs");
        machine.uptime("12.34 40.00\n");
        machine
    }

    /// A mountinfo naming `run` as a mount of `kind` on its own device,
    /// under an earlier ext4 mount at the same point.
    fn mount(&self, kind: &str) {
        let dev = fs::metadata(&self.paths.run).unwrap().dev();
        self.mountinfo(dev, kind);
    }

    fn mountinfo(&self, dev: u64, kind: &str) {
        let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
        let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
        let run = self.paths.run.display();
        fs::write(
            &self.paths.mountinfo,
            format!(
                "22 1 0:21 / / rw,relatime shared:1 - btrfs /dev/vda2 rw\n\
                 30 22 8:1 / {run} rw shared:4 - ext4 /dev/sda1 rw\n\
                 31 30 {major}:{minor} / {run} rw,nosuid,nodev shared:5 - {kind} {kind} rw,mode=755\n\
                 32 31 0:40 / {run}/user rw shared:6 - tmpfs tmpfs rw\n"
            ),
        )
        .unwrap();
    }

    fn uptime(&self, text: &str) {
        fs::write(&self.paths.uptime, text).unwrap();
    }

    fn directory(&self) -> PathBuf {
        self.paths.run.join("td-admit")
    }

    fn record_path(&self) -> PathBuf {
        self.directory().join("v1")
    }

    fn consumed(&self) -> PathBuf {
        self.directory().join("consumed")
    }

    pub(crate) fn marker(&self) -> PathBuf {
        self.root.join("var/lib/td/login/protected")
    }

    /// Writes `bytes` as `td-boot admit` would: mode 0600.
    fn write(&self, bytes: &[u8]) {
        fs::write(self.record_path(), bytes).unwrap();
        fs::set_permissions(self.record_path(), Permissions::from_mode(0o600)).unwrap();
    }

    pub(crate) fn admit(&self, uid: u32, method: Method, manifest: [u8; 32]) {
        self.write(
            &Record {
                uid,
                method,
                manifest,
            }
            .encode(),
        );
    }

    fn consume(&self) -> Outcome {
        consume(&self.paths, UID)
    }

    /// Consumption reset: no record, no consumed marker, no protected one.
    pub(crate) fn reset(&self) {
        let _ = fs::remove_file(self.record_path());
        let _ = fs::remove_file(self.consumed());
        let _ = fs::remove_file(self.marker());
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.directory(), Permissions::from_mode(0o700));
        let _ = fs::set_permissions(
            self.root.join("var/lib/td/login"),
            Permissions::from_mode(0o700),
        );
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(crate) fn id() -> [u8; 32] {
    admission::manifest_id(ID.as_bytes()).unwrap()
}

fn discarded(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Discarded(_))
}

fn protected(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Protected(_))
}

#[test]
fn the_production_paths_are_the_systems() {
    let paths = Paths::production();
    assert_eq!(paths.run, Path::new("/run"));
    assert_eq!(paths.root, Path::new("/"));
    assert_eq!(paths.owner, Owner::ROOT);
    assert_eq!(paths.mountinfo, Path::new("/proc/self/mountinfo"));
    assert_eq!(paths.uptime, Path::new("/proc/uptime"));
    assert_eq!(DEPLOYMENT, "td-deployment");
    assert_eq!(
        (admission::DIRECTORY, admission::RECORD, admission::CONSUMED),
        ("td-admit", "v1", "consumed")
    );
    assert_eq!(login_state::PROTECTED, "protected");
    // The predicate's boot check names the directory consumption reads.
    assert_eq!(
        paths.root.join(login_state::BOOT_PROTECTED),
        paths.run.join(admission::DIRECTORY)
    );
}

/// Without `td-admit` the boot is unprotected and nothing is touched; an
/// empty one, which only a protected release leaves, still makes the
/// persistent marker.
#[test]
fn no_record_is_absent_and_only_a_protected_boot_marks() {
    let machine = Machine::new();
    fs::remove_dir(machine.directory()).unwrap();
    assert_eq!(machine.consume(), Outcome::Absent);
    assert!(!machine.consumed().exists());
    assert!(!machine.marker().exists());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(machine.directory())
        .unwrap();
    assert_eq!(machine.consume(), Outcome::Absent);
    assert!(!machine.consumed().exists());
    assert!(machine.marker().exists());
}

/// A primary release's record admits once: it is unlinked, the consumed
/// marker made, the protected marker made, and a copy written back later
/// in the boot is discarded.
#[test]
fn a_primary_record_admits_once_and_marks_the_machine() {
    let machine = Machine::new();
    for method in [Method::TpmPin, Method::Fido2Primary] {
        machine.reset();
        machine.admit(UID, method, id());
        assert_eq!(machine.consume(), Outcome::Admitted, "{method:?}");
        assert!(!machine.record_path().exists());
        let consumed = fs::symlink_metadata(machine.consumed()).unwrap();
        assert!(consumed.is_file());
        assert_eq!(consumed.mode() & 0o7777, 0o600);
        let marker = fs::symlink_metadata(machine.marker()).unwrap();
        assert!(marker.is_file());
        assert_eq!(marker.mode() & 0o7777, 0o600);
        assert_eq!(marker.len(), 0);
        // The next generation finds no record.
        assert_eq!(machine.consume(), Outcome::Absent);
        // A copy written back is discarded, and still unlinked.
        machine.admit(UID, method, id());
        assert!(discarded(&machine.consume()));
        assert!(!machine.record_path().exists());
    }
}

/// A valid record that admits nothing still marks the machine protected.
#[test]
fn a_valid_record_that_admits_nothing_still_marks_the_machine() {
    let machine = Machine::new();
    let mut other = id();
    other[31] ^= 1;
    for (uid, method, manifest) in [
        (UID, Method::None, id()),
        (0, Method::TpmPin, id()),
        (1001, Method::Fido2Primary, id()),
        (UID, Method::TpmPin, other),
    ] {
        machine.reset();
        machine.admit(uid, method, manifest);
        let outcome = machine.consume();
        assert!(protected(&outcome), "{uid} {method:?}: {outcome:?}");
        assert!(machine.marker().exists());
        assert!(machine.consumed().exists());
        assert!(!machine.record_path().exists());
    }
}

/// `CLOCK_BOOTTIME` at most 300 seconds admits; past it, or unreadable,
/// admits nothing.
#[test]
fn the_record_admits_only_within_its_age() {
    let machine = Machine::new();
    for (uptime, admits) in [
        ("0.00 0.00\n", true),
        ("299.99 1.00\n", true),
        ("300.00 1.00\n", true),
        ("300.01 1.00\n", false),
        ("86400.00 1.00\n", false),
        ("12 1.00\n", false),
        ("12.3 1.00\n", false),
        ("12.345 1.00\n", false),
        ("-1.00 1.00\n", false),
        ("", false),
        ("99999999999999999999.00 1\n", false),
    ] {
        machine.reset();
        machine.uptime(uptime);
        machine.admit(UID, Method::TpmPin, id());
        let outcome = machine.consume();
        assert_eq!(outcome.admitted(), admits, "{uptime:?}: {outcome:?}");
        assert!(machine.marker().exists());
    }
    machine.reset();
    fs::remove_file(&machine.paths.uptime).unwrap();
    machine.admit(UID, Method::TpmPin, id());
    assert!(protected(&machine.consume()));
}

/// The running deployment is `td-deployment`'s exact line.
#[test]
fn the_record_admits_only_the_running_deployment() {
    let machine = Machine::new();
    let deployment = machine.paths.run.join("td-deployment");
    for text in [
        ID.to_string(),
        format!("{ID}\n\n"),
        format!("{}\n", ID.to_uppercase()),
        format!("{}\n", &ID[1..]),
        String::new(),
    ] {
        machine.reset();
        fs::write(&deployment, &text).unwrap();
        machine.admit(UID, Method::TpmPin, id());
        assert!(protected(&machine.consume()), "{text:?}");
    }
    machine.reset();
    fs::remove_file(&deployment).unwrap();
    machine.admit(UID, Method::TpmPin, id());
    assert!(protected(&machine.consume()));
}

/// A record of the wrong shape, mode, links or type is discarded: it is
/// unlinked and the consumed marker made, and, since only a protected
/// release leaves `td-admit`, the machine is marked.
#[test]
fn a_refused_record_is_consumed_and_still_marks_the_machine() {
    let machine = Machine::new();
    let valid = Record {
        uid: UID,
        method: Method::TpmPin,
        manifest: id(),
    }
    .encode();
    let mut magic = valid;
    magic[0] = b'X';
    let mut version = valid;
    version[8] = 2;
    let mut method = valid;
    method[13] = 3;
    for bytes in [
        &valid[..45],
        &[&valid[..], &[0]].concat()[..],
        &[][..],
        &magic[..],
        &version[..],
        &method[..],
    ] {
        machine.reset();
        machine.write(bytes);
        assert!(discarded(&machine.consume()), "{bytes:?}");
        assert!(!machine.record_path().exists());
        assert!(machine.consumed().exists());
        assert!(machine.marker().exists());
    }
    for mode in [0o400, 0o640, 0o644, 0o4600] {
        machine.reset();
        machine.write(&valid);
        fs::set_permissions(machine.record_path(), Permissions::from_mode(mode)).unwrap();
        assert!(discarded(&machine.consume()), "{mode:o}");
        assert!(!machine.record_path().exists());
        assert!(machine.marker().exists());
    }
    // A second link.
    machine.reset();
    machine.write(&valid);
    let other = machine.root.join("other-link");
    fs::hard_link(machine.record_path(), &other).unwrap();
    assert!(discarded(&machine.consume()));
    assert!(machine.marker().exists());
    // A link to a valid record.
    machine.reset();
    fs::remove_file(&other).unwrap();
    let elsewhere = machine.directory().join("elsewhere");
    fs::write(&elsewhere, valid).unwrap();
    fs::set_permissions(&elsewhere, Permissions::from_mode(0o600)).unwrap();
    symlink(&elsewhere, machine.record_path()).unwrap();
    assert!(discarded(&machine.consume()));
    assert!(!machine.record_path().exists());
    assert!(elsewhere.exists());
    assert!(machine.marker().exists());
    // Another owner.
    machine.reset();
    machine.write(&valid);
    let paths = Paths {
        owner: Owner {
            uid: machine.paths.owner.uid ^ 1,
            ..machine.paths.owner
        },
        run: machine.paths.run.clone(),
        root: machine.paths.root.clone(),
        mountinfo: machine.paths.mountinfo.clone(),
        uptime: machine.paths.uptime.clone(),
    };
    assert!(discarded(&consume(&paths, UID)));
    // The same owner refuses the login directory, so no marker is made.
    assert!(!machine.marker().exists());
}

/// The record must be on the tmpfs mounted at `run`, the topmost mount
/// there.
#[test]
fn a_record_off_the_run_tmpfs_is_discarded() {
    let machine = Machine::new();
    let dev = fs::metadata(&machine.paths.run).unwrap().dev();
    for broken in [
        |machine: &Machine, _| machine.mount("ext4"),
        |machine: &Machine, dev: u64| machine.mountinfo(dev ^ 1, "tmpfs"),
        |machine: &Machine, _| fs::write(&machine.paths.mountinfo, "garbage\n").unwrap(),
        |machine: &Machine, _| fs::remove_file(&machine.paths.mountinfo).unwrap(),
    ] {
        machine.reset();
        machine.mount("tmpfs");
        broken(&machine, dev);
        machine.admit(UID, Method::TpmPin, id());
        assert!(discarded(&machine.consume()));
        assert!(!machine.record_path().exists());
        assert!(machine.marker().exists());
    }
}

/// An existing consumed marker of any type discards a record; an
/// untrustworthy `td-admit` is left alone, though it marks the machine.
#[test]
fn an_earlier_consumption_or_a_bad_directory_discards() {
    let machine = Machine::new();
    fs::create_dir(machine.consumed()).unwrap();
    machine.admit(UID, Method::TpmPin, id());
    assert!(discarded(&machine.consume()));
    assert!(!machine.record_path().exists());
    assert!(machine.marker().exists());
    fs::remove_dir(machine.consumed()).unwrap();
    for mode in [0o755, 0o750, 0o1700] {
        machine.reset();
        machine.admit(UID, Method::TpmPin, id());
        fs::set_permissions(machine.directory(), Permissions::from_mode(mode)).unwrap();
        assert!(discarded(&machine.consume()), "{mode:o}");
        assert!(machine.record_path().exists());
        assert!(!machine.consumed().exists());
        fs::set_permissions(machine.directory(), Permissions::from_mode(0o700)).unwrap();
    }
    // A link in its place.
    machine.reset();
    let real = machine.paths.run.join("real");
    fs::rename(machine.directory(), &real).unwrap();
    symlink(&real, machine.directory()).unwrap();
    machine.write(
        &Record {
            uid: UID,
            method: Method::TpmPin,
            manifest: id(),
        }
        .encode(),
    );
    assert!(discarded(&machine.consume()));
    assert!(real.join("v1").exists());
    assert!(!real.join("consumed").exists());
}

/// Admission needs the protected marker: when it cannot be made, a valid
/// record is consumed and admits nothing, and a Prepare without a record
/// retries it; an existing marker of any type is kept.
#[test]
fn admission_needs_the_protected_marker() {
    let machine = Machine::new();
    let login = machine.root.join("var/lib/td/login");
    fs::set_permissions(&login, Permissions::from_mode(0o750)).unwrap();
    machine.admit(UID, Method::TpmPin, id());
    let outcome = machine.consume();
    assert!(protected(&outcome), "{outcome:?}");
    assert!(machine.consumed().exists());
    assert!(!machine.record_path().exists());
    assert!(matches!(machine.consume(), Outcome::Unmarked(_)));
    fs::set_permissions(&login, Permissions::from_mode(0o700)).unwrap();
    assert!(!machine.marker().exists());
    assert_eq!(machine.consume(), Outcome::Absent);
    assert!(machine.marker().exists());
    machine.reset();
    fs::create_dir(machine.marker()).unwrap();
    machine.admit(UID, Method::TpmPin, id());
    assert_eq!(machine.consume(), Outcome::Admitted);
    assert!(machine.marker().is_dir());
}

#[test]
fn a_mountinfo_line_names_its_device_point_and_type() {
    assert_eq!(
        mount_entry("31 30 0:25 / /run rw,nosuid shared:5 master:1 - tmpfs tmpfs rw"),
        Some((25, "/run", "tmpfs"))
    );
    assert_eq!(
        mount_entry("31 30 8:1 / /run rw - ext4 /dev/sda1 rw"),
        Some((0x801, "/run", "ext4"))
    );
    assert_eq!(device_number(259, 65537), 0x1001_0301);
    for line in [
        "",
        "31 30 0:25 / /run rw",
        "31 30 x:25 / /run rw - tmpfs t rw",
    ] {
        assert_eq!(mount_entry(line), None, "{line:?}");
    }
}
