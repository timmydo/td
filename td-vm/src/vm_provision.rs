//! One automatic provisioning attempt per QEMU supervisor lifetime.
use crate::vm_wire::workspace::{Plan, Progress};
use std::time::{Duration, Instant};

const RETRY: Duration = Duration::from_secs(2);
const UNAVAILABLE: Duration = Duration::from_secs(600);

pub trait Host {
    fn key(&mut self) -> Result<(), String>;
    fn enroll(&mut self) -> Result<Plan, String>;
    fn ensure(&mut self, plan: &Plan) -> Result<Progress, String>;
    fn report(&mut self, message: &str);
}

enum Phase {
    Key,
    Enroll,
    Clone(Box<Plan>),
    Done,
}

pub struct Provision {
    phase: Phase,
    next: Instant,
    unavailable_until: Instant,
    last: String,
}

impl Provision {
    pub fn new(now: Instant) -> Self {
        Self { phase: Phase::Key, next: now, unavailable_until: now + UNAVAILABLE, last: String::new() }
    }

    fn report(&mut self, host: &mut impl Host, message: String) {
        if self.last != message {
            host.report(&message);
            self.last = message;
        }
    }

    fn unavailable(&mut self, host: &mut impl Host, now: Instant, stage: &str, error: String) {
        if now >= self.unavailable_until {
            self.phase = Phase::Done;
            self.report(host, format!("Blocked: {stage} unavailable for ten minutes: {error}. Inspect the guest and use explicit workspace enrollment/clone to retry."));
        } else {
            // Avoid a changing transport diagnostic flooding retained logs.
            self.report(host, format!("Waiting for {stage}; inspect guest logs if this persists."));
        }
    }

    pub fn due(&self, now: Instant) -> bool { !matches!(self.phase, Phase::Done) && now >= self.next }

    pub fn poll(&mut self, host: &mut impl Host, now: Instant) {
        if now < self.next { return; }
        self.next = now + RETRY;
        match &self.phase {
            Phase::Key => match host.key() {
                Ok(()) => {
                    self.phase = Phase::Enroll;
                    self.report(host, "Enrolling the guest Git key and retaining its starting commit.".into());
                }
                Err(error) => self.unavailable(host, now, "the guest Git key", error),
            },
            Phase::Enroll => match host.enroll() {
                Ok(plan) => {
                    self.phase = Phase::Clone(Box::new(plan));
                    self.unavailable_until = now + UNAVAILABLE;
                    self.report(host, "Preparing the private guest clone and task worktree.".into());
                }
                Err(error) => {
                    self.phase = Phase::Done;
                    self.report(host, format!("Blocked: {error}. Repair the host Git profile/service, then use workspace enroll and clone to retry."));
                }
            },
            Phase::Clone(plan) => match host.ensure(plan) {
                Ok(Progress::Pending) => {
                    // Slow object transfer is not an unavailable bridge.
                    self.unavailable_until = now + UNAVAILABLE;
                    self.report(host, "Preparing the private guest clone and task worktree.".into());
                }
                Ok(Progress::Ready) => {
                    self.phase = Phase::Done;
                    self.report(host, "Prepared: /home/tester/src/td-vm/work. Terminal launch, private build-store setup and agent login remain pending.".into());
                }
                Ok(Progress::Failed(error)) => {
                    self.phase = Phase::Done;
                    self.report(host, format!("Blocked: guest workspace preparation failed: {error}. Repair the cause and use workspace clone to request a retry."));
                }
                Err(error) => self.unavailable(host, now, "guest workspace progress", error),
            },
            Phase::Done => {}
        }
    }
}

/// Cancel after reaping QEMU, close the bridge, then join before supervisor
/// exit so in-flight profile clients retain their deadline and cleanup owner.
pub struct Worker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Worker {
    pub fn start(root: std::path::PathBuf, name: String, lifetime: std::fs::File) -> Self {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&stop);
        let thread = std::thread::Builder::new().name("vm-provision".into()).spawn(move || {
            let _lifetime = lifetime;
            let result = (|| -> Result<(), String> {
                let manager = crate::Manager::existing(&root)?;
                let dir = manager.instance(&name)?;
                if crate::vm_workspace::load(&dir)?.is_none() { return Ok(()); }
                let mut provision = Provision::new(Instant::now());
                while !cancel.load(Ordering::Relaxed) && !matches!(provision.phase, Phase::Done) {
                    if provision.due(Instant::now()) {
                        // Contention is not an attempted or uncertain mutation.
                        if let Some(lock) = crate::optional_lock(&root.join("locks").join(format!("instance-{name}")), &name, false)? {
                            if cancel.load(Ordering::Relaxed) { break; }
                            let mut host = crate::ProvisionHost { manager: &manager, dir: &dir, lock: &lock };
                            provision.poll(&mut host, Instant::now());
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(())
            })();
            if let Err(error) = result { eprintln!("automatic workspace provisioning unavailable: {error}"); }
        });
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(error) => { eprintln!("start automatic workspace provisioning: {error}"); None }
        };
        Self { stop, thread }
    }
    pub fn cancel(&self) { self.stop.store(true, std::sync::atomic::Ordering::Relaxed); }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Fixture {
        key: VecDeque<Result<(), String>>,
        enrollment_error: Option<String>,
        replies: VecDeque<Result<Progress, String>>,
        calls: Vec<&'static str>,
        reports: Vec<String>,
    }
    impl Host for Fixture {
        fn key(&mut self) -> Result<(), String> {
            self.calls.push("key"); self.key.pop_front().unwrap_or(Ok(()))
        }
        fn enroll(&mut self) -> Result<Plan, String> {
            self.calls.push("enroll");
            self.enrollment_error.take().map_or_else(|| Ok(crate::vm_wire::workspace::example()), Err)
        }
        fn ensure(&mut self, plan: &Plan) -> Result<Progress, String> {
            assert_eq!(*plan, crate::vm_wire::workspace::example());
            self.calls.push("ensure"); self.replies.pop_front().unwrap_or(Ok(Progress::Pending))
        }
        fn report(&mut self, value: &str) { self.reports.push(value.into()); }
    }
    #[test]
    #[ignore = "host subprocess shutdown fixture"]
    fn shutdown_joins_blocked_capture_before_releasing_its_instance_lease() {
        use std::fs::{self, File};
        use std::process::{Command, Stdio};
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        let root = std::env::temp_dir().join(format!("td-vm-worker-exit-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(root.join("socket")).unwrap();
        let lease = File::create(root.join("lease")).unwrap(); lease.lock().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "vm_git_profile::tests::profile_child", "--ignored", "--nocapture"])
            .env("TD_VM_PROFILE_CHILD", "backlog").env("TD_VM_PROFILE_PID", root.join("pid"))
            .env("TD_VM_PROFILE_SOCKET", root.join("socket"));
        let finished = Arc::new(AtomicBool::new(false)); let completed = Arc::clone(&finished);
        let thread = std::thread::spawn(move || {
            assert!(crate::vm_git_profile::capture_until(command, Stdio::from(lease), Duration::from_millis(800)).is_err());
            completed.store(true, Ordering::Relaxed);
        });
        let worker = Worker { stop: Arc::new(AtomicBool::new(false)), thread: Some(thread) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("pid").exists() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(5)); }
        let pid = fs::read_to_string(root.join("pid")).unwrap();
        assert!(File::open(root.join("lease")).unwrap().try_lock().is_err());
        drop(worker);
        assert!(finished.load(Ordering::Relaxed));
        assert!(Instant::now() < deadline);
        assert!(!std::path::Path::new("/proc").join(pid).exists());
        assert!(File::open(root.join("lease")).unwrap().try_lock().is_ok());
        drop(listener); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn slow_clone_polls_without_reenrolling_or_restarting_and_stops_at_ready() {
        let now = Instant::now(); let mut run = Provision::new(now);
        let mut host = Fixture { key: [Err("booting".into()), Ok(())].into(),
            replies: [Ok(Progress::Pending), Err("carrier disconnected".into()), Ok(Progress::Pending), Ok(Progress::Ready)].into(), ..Fixture::default() };
        for seconds in [0, 1, 2, 4, 6, 8, 10, 900, 1800] {
            run.poll(&mut host, now + Duration::from_secs(seconds));
        }
        assert_eq!(host.calls, ["key", "key", "enroll", "ensure", "ensure", "ensure", "ensure"]);
        assert!(host.reports.last().unwrap().starts_with("Prepared:"));
    }
    #[test]
    fn failed_clones_and_unconfirmed_enrollment_wait_for_explicit_recovery() {
        for enrollment in [false, true] {
            let now = Instant::now(); let mut run = Provision::new(now);
            let mut host = Fixture { enrollment_error: enrollment.then(|| "lost registrar reply".into()),
                replies: [Ok(Progress::Failed("wrong host key".into()))].into(), ..Fixture::default() };
            for seconds in [0, 2, 4, 6, 900] { run.poll(&mut host, now + Duration::from_secs(seconds)); }
            let expected = if enrollment { vec!["key", "enroll"] } else { vec!["key", "enroll", "ensure"] };
            assert_eq!(host.calls, expected);
            assert!(host.reports.last().unwrap().starts_with("Blocked:"));
        }
    }
    #[test]
    fn missing_guest_times_out_but_a_new_supervisor_can_resume() {
        let now = Instant::now(); let mut run = Provision::new(now);
        let mut host = Fixture { key: [Err("offline".into()), Err("offline".into())].into(), ..Fixture::default() };
        for seconds in [0, 600, 1200] { run.poll(&mut host, now + Duration::from_secs(seconds)); }
        assert_eq!(host.calls, ["key", "key"]);
        assert!(host.reports.last().unwrap().starts_with("Blocked:"));
        let mut resumed = Provision::new(now);
        resumed.poll(&mut host, now);
        resumed.poll(&mut host, now + RETRY);
        assert_eq!(host.calls, ["key", "key", "key", "enroll"]);
    }
}
