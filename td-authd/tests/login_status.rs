#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::inspection::tests::{
    expiring, fixture as store_fixture, login_fixture, release, stuck, LOGIN_DAMAGED, LOGIN_EIGHT,
    LOGIN_FAILED, LOGIN_OVERSIZED, LOGIN_TWO, STALLED,
};
use std::cell::Cell;
use std::fs::{self, Permissions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

const USER: &str = "tester";

/// A root holding `var/lib/td/login` as the test's own 0700 directory, and
/// a hostname file.
pub(crate) struct Root {
    root: PathBuf,
}

impl Root {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "td-authd-login-status-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        fs::DirBuilder::new()
            .mode(0o755)
            .recursive(true)
            .create(root.join("var/lib/td"))
            .unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("var/lib/td/login"))
            .unwrap();
        fs::write(root.join("hostname"), b"td-laptop\n").unwrap();
        Self { root }
    }

    fn login(&self) -> PathBuf {
        self.root.join("var/lib/td/login")
    }

    pub(crate) fn enroll(&self) {
        fs::write(self.login().join("1000"), b"a record the helper reads").unwrap();
    }

    pub(crate) fn unenroll(&self) {
        fs::remove_file(self.login().join("1000")).unwrap();
    }

    /// A wrong mode: a damaged directory.
    pub(crate) fn damage(&self) {
        fs::set_permissions(self.login(), Permissions::from_mode(0o750)).unwrap();
    }

    fn hostname(&self, bytes: &[u8]) {
        fs::write(self.root.join("hostname"), bytes).unwrap();
    }

    /// A status reading under this root, whose helper is `helper`.
    pub(crate) fn status(&self, helper: Helper) -> Status {
        let meta = fs::metadata(&self.root).unwrap();
        let mut status = Status::new(1000, USER).unwrap();
        status.root = self.root.clone();
        status.directory_owner = Owner {
            uid: meta.uid(),
            gid: meta.gid(),
        };
        status.hostname = self.root.join("hostname");
        status.helper = helper;
        status
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.login(), Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A helper launch that plays the child fixture `selector` and counts its
/// runs.
pub(crate) fn counting(selector: u8) -> (Helper, Rc<Cell<usize>>) {
    let runs = Rc::new(Cell::new(0));
    let counted = Rc::clone(&runs);
    let helper: Helper = Box::new(move |owner| {
        assert_eq!(owner, 1000);
        counted.set(counted.get() + 1);
        Ok(login_fixture(selector))
    });
    (helper, runs)
}

/// A helper launch that must not happen.
pub(crate) fn never() -> Helper {
    Box::new(|_| panic!("the login helper ran"))
}

/// `9a`, the state's bytes, then this module's names and the reserved
/// revocation byte.
pub(crate) fn answer(state: &[u8], hostname: &str) -> Vec<u8> {
    let mut bytes = vec![0x9a];
    bytes.extend_from_slice(state);
    bytes.push(USER.len() as u8);
    bytes.extend_from_slice(USER.as_bytes());
    bytes.push(hostname.len() as u8);
    bytes.extend_from_slice(hostname.as_bytes());
    bytes.push(0);
    bytes
}

const DIRECTORY_DAMAGED: [u8; 2] = [2, 0x0a];
const RECORD_DAMAGED: [u8; 2] = [2, 0x0b];
const UNREADABLE: [u8; 2] = [2, 0x0c];

#[test]
fn an_unenrolled_machine_never_runs_the_helper() {
    let root = Root::new();
    let mut status = root.status(never());
    let unenrolled = answer(&[0], "td-laptop");
    assert_eq!(
        unenrolled,
        [&[0x9a, 0, 6][..], b"tester", &[9], b"td-laptop", &[0][..]].concat()
    );
    for _ in 0..3 {
        assert_eq!(status.answer(false).unwrap(), unenrolled);
    }
    // Other names, temporaries and another account's record are not ours.
    for name in ["tmp-0123", "cutover-reboot", "1001"] {
        fs::write(root.login().join(name), b"").unwrap();
    }
    status.operation_ended();
    assert_eq!(status.answer(false).unwrap(), unenrolled);
}

#[test]
fn a_damaged_directory_is_unavailable_without_the_helper() {
    let root = Root::new();
    let mut status = root.status(never());
    fs::set_permissions(root.login(), Permissions::from_mode(0o750)).unwrap();
    assert_eq!(
        status.answer(false).unwrap(),
        answer(&DIRECTORY_DAMAGED, "td-laptop")
    );
    fs::remove_dir(root.login()).unwrap();
    status.operation_ended();
    assert_eq!(
        status.answer(false).unwrap(),
        answer(&DIRECTORY_DAMAGED, "td-laptop")
    );
}

#[test]
fn an_enrolled_answer_carries_the_helpers_keys_without_its_version() {
    let root = Root::new();
    root.enroll();
    let (helper, runs) = counting(LOGIN_TWO);
    let mut status = root.status(helper);
    let enrolled = answer(&[&[1, 2][..], &[0xa1; 4], &[0xa2; 4]].concat(), "td-laptop");
    assert_eq!(status.answer(false).unwrap(), enrolled);
    assert_eq!(runs.get(), 1);
    let (helper, _) = counting(LOGIN_EIGHT);
    let mut status = root.status(helper);
    let mut keys = vec![1, 8];
    for key in 1..=8 {
        keys.extend_from_slice(&[key; 4]);
    }
    assert_eq!(status.answer(false).unwrap(), answer(&keys, "td-laptop"));
    let (helper, _) = counting(LOGIN_DAMAGED);
    assert_eq!(
        root.status(helper).answer(false).unwrap(),
        answer(&RECORD_DAMAGED, "td-laptop")
    );
}

#[test]
fn a_helper_that_fails_in_any_way_reads_as_could_not_be_read() {
    let root = Root::new();
    root.enroll();
    // A store result, an oversized one, a failed exit, and a child that
    // writes a result and then fails.
    for selector in [3, LOGIN_OVERSIZED, LOGIN_FAILED, 7] {
        let (helper, runs) = counting(selector);
        assert_eq!(
            root.status(helper).answer(false).unwrap(),
            answer(&UNREADABLE, "td-laptop"),
            "{selector}"
        );
        assert_eq!(runs.get(), 1);
    }
    // A store-bounded helper reading the login result.
    let helper: Helper = Box::new(|_| Ok(store_fixture(LOGIN_TWO)));
    assert_eq!(
        root.status(helper).answer(false).unwrap(),
        answer(&UNREADABLE, "td-laptop")
    );
    // A helper that cannot start: the production launch, whose
    // /bin/td-secret this host does not have, and a refused launch.
    assert!(!Path::new("/bin/td-secret").exists());
    for helper in [
        Box::new(Inspection::login) as Helper,
        Box::new(|_| Err("refused".to_string())),
    ] {
        assert_eq!(
            root.status(helper).answer(false).unwrap(),
            answer(&UNREADABLE, "td-laptop")
        );
    }
}

#[test]
fn a_slow_helper_is_unreadable_at_its_two_second_deadline() {
    let root = Root::new();
    root.enroll();
    let (helper, runs) = counting(STALLED);
    let mut status = root.status(helper);
    let started = Instant::now();
    assert_eq!(
        status.answer(false).unwrap(),
        answer(&UNREADABLE, "td-laptop")
    );
    let waited = started.elapsed();
    assert!(waited >= Duration::from_secs(2), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    assert_eq!(runs.get(), 1);
}

#[test]
fn the_cache_holds_until_a_login_operation_ends() {
    let root = Root::new();
    root.enroll();
    let (helper, runs) = counting(LOGIN_TWO);
    let mut status = root.status(helper);
    let enrolled = answer(&[&[1, 2][..], &[0xa1; 4], &[0xa2; 4]].concat(), "td-laptop");
    assert_eq!(status.answer(false).unwrap(), enrolled);
    // A later answer is the cache's, whatever the disk now holds.
    root.unenroll();
    assert_eq!(status.answer(false).unwrap(), enrolled);
    assert_eq!(runs.get(), 1);
    // A login operation's end reads afresh, here with no helper.
    status.operation_ended();
    assert_eq!(status.answer(false).unwrap(), answer(&[0], "td-laptop"));
    root.enroll();
    assert_eq!(status.answer(false).unwrap(), answer(&[0], "td-laptop"));
    status.operation_ended();
    assert_eq!(status.answer(false).unwrap(), enrolled);
    assert_eq!(runs.get(), 2);
}

#[test]
fn while_unreadable_the_helper_runs_at_most_once_in_two_seconds() {
    let root = Root::new();
    root.enroll();
    let selector = Rc::new(Cell::new(LOGIN_FAILED));
    let runs = Rc::new(Cell::new(0));
    let (chosen, counted) = (Rc::clone(&selector), Rc::clone(&runs));
    let mut status = root.status(Box::new(move |_| {
        counted.set(counted.get() + 1);
        Ok(login_fixture(chosen.get()))
    }));
    assert_eq!(
        status.answer(false).unwrap(),
        answer(&UNREADABLE, "td-laptop")
    );
    assert_eq!(runs.get(), 1);
    // The compositor's polls in between are answered from the cache at
    // once, a login operation's end included; the hostname is re-read.
    selector.set(LOGIN_TWO);
    root.hostname(b"other\n");
    for _ in 0..4 {
        let started = Instant::now();
        assert_eq!(status.answer(false).unwrap(), answer(&UNREADABLE, "other"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }
    status.operation_ended();
    assert_eq!(status.answer(false).unwrap(), answer(&UNREADABLE, "other"));
    assert_eq!(runs.get(), 1);
    // Two seconds after the last run ended, it runs again.
    status.helped = Some(Instant::now() - HELPER_PAUSE);
    let enrolled = answer(&[&[1, 2][..], &[0xa1; 4], &[0xa2; 4]].concat(), "other");
    assert_eq!(status.answer(false).unwrap(), enrolled);
    assert_eq!(runs.get(), 2);
    // Resolved: the cache answers without any run.
    assert_eq!(status.answer(false).unwrap(), enrolled);
    assert_eq!(runs.get(), 2);
    // The predicate needs no helper, so a removed record resolves at once.
    selector.set(LOGIN_FAILED);
    status.operation_ended();
    assert_eq!(status.answer(false).unwrap(), answer(&UNREADABLE, "other"));
    assert_eq!(runs.get(), 3);
    root.unenroll();
    assert_eq!(status.answer(false).unwrap(), answer(&[0], "other"));
    assert_eq!(runs.get(), 3);
}

/// `seconds` before now.
fn ago(seconds: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(seconds))
        .unwrap()
}

/// Ticks until a retained helper is reaped.
fn settle(status: &mut Status) {
    // Returns once the killed helper is reaped; the bound only stops a hang.
    let until = Instant::now() + Duration::from_secs(10);
    while status.running.is_some() {
        assert!(Instant::now() < until);
        status.tick();
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_helper_still_unreaped_is_never_joined_by_a_second() {
    let root = Root::new();
    root.enroll();
    let runs = Rc::new(Cell::new(0));
    let counted = Rc::clone(&runs);
    let mut status = root.status(Box::new(move |_| {
        counted.set(counted.get() + 1);
        Ok(stuck(Duration::from_millis(100)))
    }));
    let started = Instant::now();
    assert_eq!(
        status.answer(false).unwrap(),
        answer(&UNREADABLE, "td-laptop")
    );
    let waited = started.elapsed();
    assert!(waited < Duration::from_millis(1500), "{waited:?}");
    assert_eq!(runs.get(), 1);
    assert!(status.running.is_some());
    // Past every pause, the killed helper still holds its place: the
    // cached could-not-be-read, at once, and no second helper.
    for _ in 0..3 {
        status.helped = Some(ago(20));
        status.tick();
        let started = Instant::now();
        assert_eq!(
            status.answer(false).unwrap(),
            answer(&UNREADABLE, "td-laptop")
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(runs.get(), 1);
    }
    release(status.running.as_mut().unwrap());
    settle(&mut status);
    status.helped = Some(ago(20));
    assert_eq!(
        status.answer(false).unwrap(),
        answer(&UNREADABLE, "td-laptop")
    );
    assert_eq!(runs.get(), 2);
    release(status.running.as_mut().unwrap());
    settle(&mut status);
}

#[test]
fn each_deadline_miss_doubles_the_pause_to_sixteen_seconds_and_an_answer_resets_it() {
    let root = Root::new();
    root.enroll();
    let selector = Rc::new(Cell::new(STALLED));
    let runs = Rc::new(Cell::new(0));
    let (chosen, counted) = (Rc::clone(&selector), Rc::clone(&runs));
    let mut status = root.status(Box::new(move |_| {
        counted.set(counted.get() + 1);
        // Only the stalled helper may miss: one that answers gets a deadline
        // a loaded machine's exec cannot outrun.
        let after = if chosen.get() == STALLED {
            Duration::from_millis(20)
        } else {
            Duration::from_secs(10)
        };
        Ok(expiring(chosen.get(), after))
    }));
    let unreadable = answer(&UNREADABLE, "td-laptop");
    assert_eq!(status.answer(false).unwrap(), unreadable);
    assert_eq!(runs.get(), 1);
    // After the first miss, the second, the third, and every later one.
    for (run, pause) in [2, 4, 8, 16, 16].into_iter().enumerate() {
        assert_eq!(status.pause(), Duration::from_secs(pause));
        settle(&mut status);
        status.helped = Some(ago(pause) + Duration::from_secs(1));
        assert_eq!(status.answer(false).unwrap(), unreadable);
        assert_eq!(runs.get(), run + 1, "paused {pause}");
        status.helped = Some(ago(pause));
        assert_eq!(status.answer(false).unwrap(), unreadable);
        assert_eq!(runs.get(), run + 2, "resumed after {pause}");
    }
    // A run the helper ended itself resets it, even a failed one.
    selector.set(LOGIN_FAILED);
    settle(&mut status);
    status.helped = Some(ago(16));
    assert_eq!(status.answer(false).unwrap(), unreadable);
    assert_eq!(runs.get(), 7);
    assert_eq!(status.pause(), HELPER_PAUSE);
    // And a miss after it starts again at two seconds.
    selector.set(STALLED);
    settle(&mut status);
    status.helped = Some(ago(2));
    assert_eq!(status.answer(false).unwrap(), unreadable);
    assert_eq!(status.pause(), HELPER_PAUSE);
    settle(&mut status);
    status.helped = Some(ago(2));
    assert_eq!(status.answer(false).unwrap(), unreadable);
    assert_eq!(status.pause(), 2 * HELPER_PAUSE);
    // The answer itself resets it too.
    selector.set(LOGIN_TWO);
    settle(&mut status);
    status.helped = Some(ago(4));
    let enrolled = answer(&[&[1, 2][..], &[0xa1; 4], &[0xa2; 4]].concat(), "td-laptop");
    assert_eq!(status.answer(false).unwrap(), enrolled);
    assert_eq!(status.misses, 0);
    assert_eq!(status.pause(), HELPER_PAUSE);
    assert_eq!(runs.get(), 10);
}

#[test]
fn a_live_boot_is_unenrolled_without_the_predicate_or_the_helper() {
    let root = Root::new();
    root.enroll();
    let mut status = root.status(never());
    assert_eq!(status.answer(true).unwrap(), answer(&[0], "td-laptop"));
    fs::remove_dir_all(root.login()).unwrap();
    assert_eq!(status.answer(true).unwrap(), answer(&[0], "td-laptop"));
    assert!(status.cached.is_none());
}

#[test]
fn the_hostname_is_sent_only_under_firstboots_rules() {
    let root = Root::new();
    let mut status = root.status(never());
    let longest = "a".repeat(63);
    for (bytes, sent) in [
        (&b"td-laptop\n"[..], "td-laptop"),
        (b"td-laptop", "td-laptop"),
        (b"configured.host\n", "configured.host"),
        (format!("{longest}\n").as_bytes(), longest.as_str()),
        (longest.as_bytes(), longest.as_str()),
        // One newline only, never another.
        (b"td\n\n", ""),
        (b"(none)\n", ""),
        (b"TD\n", ""),
        (b"1host\n", ""),
        (b"host-\n", ""),
        (b"a b\n", ""),
        (b"h\xc3\xb6st\n", ""),
        (b"\xff\n", ""),
        (b"", ""),
        (b"\n", ""),
        (format!("{longest}a\n").as_bytes(), ""),
        (format!("{longest}aa").as_bytes(), ""),
        ("a".repeat(200).as_bytes(), ""),
    ] {
        root.hostname(bytes);
        assert_eq!(
            status.answer(false).unwrap(),
            answer(&[0], sent),
            "{bytes:?}"
        );
    }
    fs::remove_file(root.root.join("hostname")).unwrap();
    assert_eq!(status.answer(false).unwrap(), answer(&[0], ""));
    fs::create_dir(root.root.join("hostname")).unwrap();
    assert_eq!(status.answer(false).unwrap(), answer(&[0], ""));
}

#[test]
fn only_a_primary_account_name_is_carried() {
    assert_eq!(Status::new(1000, "alice").unwrap().username, "alice");
    assert!(Status::new(1000, &"a".repeat(32)).is_ok());
    for name in ["", "1abc", "_abc", "-abc", "Alice", "a.b", &"a".repeat(33)] {
        assert!(Status::new(1000, name).is_err(), "{name:?}");
    }
}
