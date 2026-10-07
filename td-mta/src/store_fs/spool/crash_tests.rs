#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::{
    limits::Limits,
    ports::Time,
    store_fs::{tests::Fixture, Directory, PrivateRoot},
};
use std::{
    io::BufRead,
    process::{Child, Command, Stdio},
    time::Duration,
};
struct Fixed;
impl Clock for Fixed {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(1),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
#[test]
#[ignore = "owned child fixture for process-death cleanup"]
fn ingress_crash_child() {
    let path = std::env::var("TD_MTA_INGRESS_FIXTURE").unwrap();
    let directory = Directory::from_path(&path).unwrap();
    let lock = super::super::acquire_lock(&directory, directory.metadata().unwrap().uid()).unwrap();
    let mut root = LockedRoot {
        root: PrivateRoot { directory },
        _lock: lock,
    };
    let spool = IngressSpool::open(
        &mut root,
        &Limits::default().plan().unwrap(),
        Arc::new(Fixed),
        deadline(),
    )
    .unwrap();
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            AccountId::from_bytes([1; 16]),
            BlobId::from_bytes([2; 16]),
            BlobKind::Message,
            deadline(),
        )
        .unwrap();
    writer.write(b"unacknowledged bytes").unwrap();
    println!("td-ingress-ready");
    io::stdout().flush().unwrap();
    let mut byte = [0];
    io::stdin().read_exact(&mut byte).unwrap();
    panic!("child unexpectedly resumed");
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn process_death_releases_lock_and_startup_discards_unacknowledged_bytes() {
    let fixture = Fixture::new();
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store_fs::spool::crash_tests::ingress_crash_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("TD_MTA_INGRESS_FIXTURE", &fixture.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut stdout = io::BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(count) = stdout.read_line(&mut line) {
            if count == 0 {
                break;
            }
            if line.trim_end().ends_with("td-ingress-ready") {
                let _ = send.send(());
                break;
            }
            line.clear();
        }
    });
    let ready = receive.recv_timeout(Duration::from_secs(10));
    if ready.is_err() {
        let _ = child.0.kill();
        let _ = child.0.wait();
    }
    reader.join().unwrap();
    ready.unwrap();
    assert!(matches!(fixture.lock(), Err(super::super::LockError::Busy)));
    child.0.kill().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(child.0.wait().unwrap().signal(), Some(9));
    assert_eq!(
        fs::read(fixture.path.join("slot-00")).unwrap(),
        b"unacknowledged bytes"
    );
    let mut root = fixture.locked();
    let spool = IngressSpool::open(
        &mut root,
        &Limits::default().plan().unwrap(),
        Arc::new(Fixed),
        deadline(),
    )
    .unwrap();
    assert_eq!(spool.status().unwrap().reserved_bytes, 0);
    assert!(!fixture.path.join("slot-00").exists());
}
