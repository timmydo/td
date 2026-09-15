//! Fresh bounded preparation and delivery through retained procfs directories.
use crate::action_plan;
use crate::actions::{Delivery, Details, Intent, Results, Scope};
use crate::budget::{Budget, MemoryVec};
use crate::format::Text;
use crate::hierarchy::{Input, ProcessKey, ROWS};
use crate::linux_read::Reader;
use crate::parsers::{self, PROCESS_BYTES};
use crate::signal_sys;
use std::fmt::Write;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

const ESRCH: i32 = 3;
const EPERM: i32 = 1;
const EACCES: i32 = 13;

#[derive(Debug)]
struct Target {
    directory: File,
    observed: Input,
}
#[derive(Debug)]
pub(crate) struct Prepared {
    intent: Intent,
    targets: MemoryVec<Target>,
    deliveries: MemoryVec<Delivery>,
}
pub(crate) struct Linux {
    budget: Arc<Budget>,
    reader: Reader,
    own_pid: u32,
    generation: u64,
}
fn cancelled(active: &AtomicU64, revision: u64) -> io::Result<()> {
    if revision == 0 || active.load(Ordering::Acquire) != revision {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "process action cancelled",
        ))
    } else {
        Ok(())
    }
}
impl Linux {
    pub(crate) fn new(budget: &Arc<Budget>, generation: u64) -> io::Result<Self> {
        if generation == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut this = Self {
            budget: Arc::clone(budget),
            reader: Reader::new(budget, PROCESS_BYTES)?,
            own_pid: std::process::id(),
            generation,
        };
        let directory = this.caller_directory()?;
        signal_sys::probe(&directory)?;
        Ok(this)
    }
    fn caller_directory(&mut self) -> io::Result<File> {
        // /proc/self identifies this process even if procfs shows an ancestor view.
        let directory = File::open("/proc/self")?;
        let process = self.identity(&directory)?;
        let status = action_plan::status(self.reader.process_file(
            &directory,
            "status",
            PROCESS_BYTES,
        )?)
        .map_err(io::Error::other)?;
        action_plan::caller(status, self.own_pid).map_err(io::Error::other)?;
        if process.key.pid != self.own_pid {
            return Err(io::Error::other("inconsistent caller PID view"));
        }
        Ok(directory)
    }
    fn identity(&mut self, directory: &File) -> io::Result<Input> {
        let process =
            parsers::process(self.reader.process_file(directory, "stat", PROCESS_BYTES)?)
                .map_err(io::Error::other)?;
        Ok(Input {
            key: ProcessKey {
                generation: self.generation,
                pid: process.pid,
                start_ticks: process.start_ticks,
            },
            parent_pid: Some(
                process
                    .parent
                    .ok_or_else(|| io::Error::other("process parent is unavailable"))?,
            ),
            cpu: None,
            rss: None,
        })
    }
    fn scan(&mut self, intent: Intent, active: &AtomicU64) -> io::Result<MemoryVec<Input>> {
        let mut rows = MemoryVec::new(&self.budget, 128).map_err(io::Error::other)?;
        for (count, entry) in std::fs::read_dir("/proc")?.enumerate() {
            cancelled(active, intent.revision)?;
            if count >= ROWS + 4096 {
                return Err(io::Error::other("process enumeration exceeds bound"));
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(pid) = parsers::unsigned(name.as_bytes())
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
            else {
                continue;
            };
            if rows.len() >= ROWS {
                return Err(io::Error::other("process roster exceeds bound"));
            }
            let observation = self
                .reader
                .process_directory(pid)
                .and_then(|directory| self.identity(&directory));
            let Some(row) = scan_observation(pid, intent.key.pid, observation)? else {
                continue;
            };
            if row.key.pid != pid {
                return Err(io::Error::other(
                    "process identity changed during enumeration",
                ));
            }
            if rows.len() == rows.capacity() {
                rows.reserve((rows.capacity() * 2).min(ROWS))
                    .map_err(io::Error::other)?;
            }
            rows.push(row)
                .map_err(|_| io::Error::other("process roster exceeds bound"))?;
        }
        rows.sort_unstable_by_key(|row| row.key.pid);
        if rows.windows(2).any(|pair| {
            pair.first()
                .zip(pair.get(1))
                .is_some_and(|(a, b)| a.key.pid == b.key.pid)
        }) {
            return Err(io::Error::other(
                "process identity changed during enumeration",
            ));
        }
        Ok(rows)
    }
    fn detail(
        &mut self,
        directory: &File,
        observed: Input,
        active: &AtomicU64,
        revision: u64,
    ) -> io::Result<Text<4096>> {
        cancelled(active, revision)?;
        let status = action_plan::status(self.reader.process_file(
            directory,
            "status",
            PROCESS_BYTES,
        )?)
        .map_err(io::Error::other)?;
        if status.pid != observed.key.pid {
            return Err(io::Error::other("process status identity changed"));
        }
        if let Err(reason) = action_plan::protected(status, self.own_pid) {
            return Err(io::Error::other(format!("PID {}: {reason}", status.pid)));
        }
        cancelled(active, revision)?;
        let mut row = Text::<4096>::default();
        write!(
            row,
            "PID {} start {} UID {}: ",
            status.pid, observed.key.start_ticks, status.uid
        )
        .map_err(io::Error::other)?;
        match self.reader.process_command(directory) {
            Ok((bytes, truncated)) => {
                let (command, shortened) = command_text(bytes);
                if bytes.is_empty() {
                    let stat = self.reader.process_file(directory, "stat", PROCESS_BYTES)?;
                    let name = parsers::process(stat).map_err(io::Error::other)?.name;
                    let (name, _) = command_text(name);
                    write!(row, "[{}; empty command line]", name.as_str())
                        .map_err(io::Error::other)?;
                } else {
                    row.write_str(command.as_str()).map_err(io::Error::other)?;
                }
                if truncated || shortened {
                    row.write_str(" [truncated]").map_err(io::Error::other)?;
                }
            }
            Err(error) => {
                write!(row, "[command unavailable: {:?}]", error.kind())
                    .map_err(io::Error::other)?;
            }
        }
        Ok(row)
    }
    pub(crate) fn prepare(
        &mut self,
        intent: Intent,
        active: &AtomicU64,
    ) -> io::Result<(Prepared, Details)> {
        cancelled(active, intent.revision)?;
        if intent.key.generation != self.generation {
            return Err(io::Error::other(
                "selection belongs to another collection session",
            ));
        }
        self.caller_directory()?;
        let mut targets =
            MemoryVec::new(&self.budget, crate::actions::TARGETS).map_err(io::Error::other)?;
        if intent.scope == Scope::Selected {
            let directory = self
                .reader
                .process_directory(intent.key.pid)
                .map_err(selected_error)?;
            let observed = self.identity(&directory).map_err(selected_error)?;
            if observed.key != intent.key {
                return Err(io::Error::other("selected process identity changed"));
            }
            targets
                .push(Target {
                    directory,
                    observed,
                })
                .map_err(|_| io::Error::other("action target limit"))?;
        } else {
            let rows = self.scan(intent, active)?;
            let mut order = action_plan::members(&self.budget, &rows, intent.key, intent.scope)
                .map_err(io::Error::other)?;
            if !intent.signal.parent_first() {
                order.reverse();
            }
            for index in order.iter().copied() {
                cancelled(active, intent.revision)?;
                let expected = *rows
                    .get(index)
                    .ok_or_else(|| io::Error::other("invalid observed member"))?;
                let directory = self
                    .reader
                    .process_directory(expected.key.pid)
                    .map_err(|error| member_error(expected.key.pid, error))?;
                let observed = self
                    .identity(&directory)
                    .map_err(|error| member_error(expected.key.pid, error))?;
                if observed != expected {
                    return Err(io::Error::other(
                        "observed subtree changed before preparation",
                    ));
                }
                targets
                    .push(Target {
                        directory,
                        observed,
                    })
                    .map_err(|_| io::Error::other("action target limit"))?;
            }
        }
        let mut details = MemoryVec::new(&self.budget, targets.len()).map_err(io::Error::other)?;
        let mut deliveries =
            MemoryVec::new(&self.budget, targets.len()).map_err(io::Error::other)?;
        for _ in targets.iter() {
            deliveries
                .push(Delivery::Cancelled)
                .map_err(|_| io::Error::other("action result limit"))?;
        }
        for target in targets.iter() {
            let row = self
                .detail(&target.directory, target.observed, active, intent.revision)
                .map_err(|error| member_error(target.observed.key.pid, error))?;
            details
                .push(row)
                .map_err(|_| io::Error::other("action detail limit"))?;
        }
        for target in targets.iter() {
            cancelled(active, intent.revision)?;
            if self
                .identity(&target.directory)
                .map_err(|error| member_error(target.observed.key.pid, error))?
                != target.observed
            {
                return Err(io::Error::other(
                    "observed subtree changed before confirmation",
                ));
            }
        }
        Ok((
            Prepared {
                intent,
                targets,
                deliveries,
            },
            Details {
                intent,
                rows: details,
            },
        ))
    }
    pub(crate) fn deliver(&mut self, mut prepared: Prepared, active: &AtomicU64) -> Results {
        for (target, delivery) in prepared.targets.iter().zip(prepared.deliveries.iter_mut()) {
            let result = if cancelled(active, prepared.intent.revision).is_err() {
                Delivery::Cancelled
            } else {
                match signal_sys::send(&target.directory, prepared.intent.signal) {
                    Ok(()) => Delivery::Sent,
                    Err(error) => match error.raw_os_error() {
                        Some(ESRCH) => Delivery::Exited,
                        Some(EPERM | EACCES) => Delivery::Permission,
                        code => Delivery::Error(code.unwrap_or(0)),
                    },
                }
            };
            // Every result slot exists before confirmation; sending cannot grow it.
            *delivery = result;
        }
        Results {
            revision: prepared.intent.revision,
            deliveries: prepared.deliveries,
        }
    }
}
fn selected_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(ESRCH) {
        io::Error::new(
            io::ErrorKind::NotFound,
            "selected process exited before preparation",
        )
    } else {
        io::Error::new(
            error.kind(),
            format!("selected process unavailable: {error}"),
        )
    }
}
fn scan_observation(
    pid: u32,
    selected: u32,
    observation: io::Result<Input>,
) -> io::Result<Option<Input>> {
    match observation {
        Ok(row) => Ok(Some(row)),
        Err(error) if pid == selected => Err(selected_error(error)),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(ESRCH) =>
        {
            Ok(None)
        }
        Err(error) => Err(io::Error::new(error.kind(), format!("PID {pid}: {error}"))),
    }
}
fn member_error(pid: u32, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("Captured PID {pid}: {error}"))
}
/// Text is display-only, with NUL argument separators and escaped control bytes.
fn command_text(mut bytes: &[u8]) -> (Text<3800>, bool) {
    let mut text = Text::<3800>::default();
    if bytes.last() == Some(&0) {
        bytes = bytes.get(..bytes.len().saturating_sub(1)).unwrap_or(&[]);
    }
    while !bytes.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(bytes) {
            Ok(valid) => (valid, 0),
            Err(error) => {
                let valid = bytes
                    .get(..error.valid_up_to())
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or("");
                (
                    valid,
                    error
                        .error_len()
                        .unwrap_or(bytes.len().saturating_sub(error.valid_up_to())),
                )
            }
        };
        for scalar in valid.chars() {
            let result = if scalar == '\0' {
                text.write_str(" | ")
            } else if scalar.is_control() {
                write!(text, "\\u{{{:x}}}", scalar as u32)
            } else {
                text.write_char(scalar)
            };
            if result.is_err() {
                return (text, true);
            }
        }
        bytes = bytes.get(valid.len()..).unwrap_or(&[]);
        for byte in bytes.iter().take(invalid) {
            if write!(text, "\\x{byte:02x}").is_err() {
                return (text, true);
            }
        }
        bytes = bytes.get(invalid..).unwrap_or(&[]);
    }
    (text, false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::actions::Signal;
    use std::io::{BufRead, Write as _};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn child() -> OwnedChild {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "action_linux::tests::owned_child_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("TD_TASKMGR_OWNED_CHILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let child = OwnedChild(child);
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(output);
            for _ in 0..32 {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.len() > 4096 {
                    return;
                }
                if line.starts_with("TD-READY ") {
                    let _ = send.send(line);
                    return;
                }
            }
        });
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(3)).unwrap().trim(),
            format!("TD-READY {}", child.0.id())
        );
        child
    }
    #[test]
    #[ignore = "owned child helper, started only by a process fixture"]
    fn owned_child_helper() {
        if std::env::var("TD_TASKMGR_OWNED_CHILD").as_deref() != Ok("1") {
            return;
        }
        println!("TD-READY {}", std::process::id());
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::park();
        }
    }
    fn setup() -> Linux {
        Linux::new(&Budget::new(crate::budget::LIMIT).unwrap(), 1).unwrap()
    }
    fn intent(linux: &mut Linux, pid: u32, revision: u64, signal: Signal) -> Intent {
        let directory = linux.reader.process_directory(pid).unwrap();
        let key = linux.identity(&directory).unwrap().key;
        Intent {
            key,
            revision,
            signal,
            scope: Scope::Selected,
        }
    }
    fn wait_state(linux: &mut Linux, pid: u32, stopped: bool) {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let directory = linux.reader.process_directory(pid).unwrap();
            let state = parsers::process(
                linux
                    .reader
                    .process_file(&directory, "stat", PROCESS_BYTES)
                    .unwrap(),
            )
            .unwrap()
            .state;
            if matches!(state, b'T' | b't') == stopped {
                return;
            }
            assert!(Instant::now() < until, "child state {state}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn wait_exit(child: &mut OwnedChild) {
        let until = Instant::now() + Duration::from_secs(3);
        while child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < until, "owned child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn owned_process_stop_continue_terminate_and_force_kill() {
        let mut linux = setup();
        let mut child = child();
        for (revision, signal) in [
            (1, Signal::Stop),
            (2, Signal::Continue),
            (3, Signal::Terminate),
        ] {
            let active = AtomicU64::new(revision);
            let request = intent(&mut linux, child.0.id(), revision, signal);
            let (prepared, details) = linux.prepare(request, &active).unwrap();
            assert_eq!(details.rows.len(), 1);
            assert!(details
                .rows
                .first()
                .unwrap()
                .as_str()
                .contains(&format!("PID {}", child.0.id())));
            let results = linux.deliver(prepared, &active);
            assert_eq!(&*results.deliveries, &[Delivery::Sent]);
            if signal == Signal::Stop {
                wait_state(&mut linux, child.0.id(), true);
            }
            if signal == Signal::Continue {
                wait_state(&mut linux, child.0.id(), false);
            }
        }
        wait_exit(&mut child);
        let mut killed = self::child();
        let active = AtomicU64::new(4);
        let request = intent(&mut linux, killed.0.id(), 4, Signal::Kill);
        let (prepared, _) = linux.prepare(request, &active).unwrap();
        assert_eq!(
            &*linux.deliver(prepared, &active).deliveries,
            &[Delivery::Sent]
        );
        wait_exit(&mut killed);
    }
    #[test]
    fn stale_descriptors_cancellation_key_mismatch_and_protected_self_refuse_retargeting() {
        let mut linux = setup();
        let mut child = child();
        let active = AtomicU64::new(1);
        let request = intent(&mut linux, child.0.id(), 1, Signal::Kill);
        let mut mismatch = request;
        mismatch.key.start_ticks += 1;
        assert!(linux.prepare(mismatch, &active).is_err());
        let own_pid = linux.own_pid;
        let own = intent(&mut linux, own_pid, 1, Signal::Stop);
        assert!(linux
            .prepare(own, &active)
            .unwrap_err()
            .to_string()
            .contains("task manager is protected"));
        let mut subtree = own;
        subtree.scope = Scope::Descendants;
        assert!(linux
            .prepare(subtree, &active)
            .unwrap_err()
            .to_string()
            .contains("task manager is protected"));
        let (prepared, _) = linux.prepare(request, &active).unwrap();
        active.store(0, Ordering::Release);
        assert_eq!(
            &*linux.deliver(prepared, &active).deliveries,
            &[Delivery::Cancelled]
        );
        assert!(child.0.try_wait().unwrap().is_none());
        active.store(1, Ordering::Release);
        let (prepared, _) = linux.prepare(request, &active).unwrap();
        child.0.kill().unwrap();
        wait_exit(&mut child);
        assert_eq!(
            &*linux.deliver(prepared, &active).deliveries,
            &[Delivery::Exited]
        );
        assert!(signal_sys::probe(&File::open("/dev/null").unwrap()).is_err());
    }
    #[test]
    fn vanished_scan_entries_are_skipped_but_selected_and_unreadable_entries_refuse() {
        for code in [2, 3] {
            assert!(
                scan_observation(43, 42, Err(io::Error::from_raw_os_error(code)))
                    .unwrap()
                    .is_none()
            );
            assert!(
                scan_observation(42, 42, Err(io::Error::from_raw_os_error(code)))
                    .unwrap_err()
                    .to_string()
                    .contains("selected process exited")
            );
        }
        assert!(
            scan_observation(43, 42, Err(io::Error::from_raw_os_error(13)))
                .unwrap_err()
                .to_string()
                .contains("PID 43")
        );
        let (text, _) = command_text(b"alpha\0beta\0");
        assert_eq!(text.as_str(), "alpha | beta");
    }
    #[test]
    fn command_detail_escapes_controls_invalid_utf8_and_marks_output_limits() {
        let (text, truncated) = command_text(b"alpha\0beta\n\xff");
        assert_eq!(text.as_str(), "alpha | beta\\u{a}\\xff");
        assert!(!truncated);
        let (text, truncated) = command_text(&[0xff; 4096]);
        assert_eq!(text.as_str().len(), 3800);
        assert!(truncated);
    }
    #[test]
    #[ignore = "owned family helper, started only by a process fixture"]
    fn owned_family_helper() {
        if std::env::var("TD_TASKMGR_OWNED_FAMILY").as_deref() != Ok("1") {
            return;
        }
        let mut children = vec![child()];
        println!("TD-MEMBER {}", children.first().unwrap().0.id());
        std::io::stdout().flush().unwrap();
        for line in std::io::stdin().lock().lines() {
            match line.unwrap().as_str() {
                "grow" => {
                    children.push(child());
                    println!("TD-MEMBER {}", children.last().unwrap().0.id());
                }
                "retire" => {
                    let mut child = children.remove(0);
                    child.0.kill().unwrap();
                    child.0.wait().unwrap();
                    println!("TD-RETIRED");
                }
                _ => break,
            }
            std::io::stdout().flush().unwrap();
        }
    }
    struct Family {
        parent: OwnedChild,
        input: std::process::ChildStdin,
        output: std::sync::mpsc::Receiver<String>,
        pinned: Vec<File>,
    }
    impl Drop for Family {
        fn drop(&mut self) {
            for directory in &self.pinned {
                let _ = signal_sys::send(directory, Signal::Kill);
            }
        }
    }
    impl Family {
        fn new() -> Self {
            let mut parent = OwnedChild(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "action_linux::tests::owned_family_helper",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("TD_TASKMGR_OWNED_FAMILY", "1")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap(),
            );
            let input = parent.0.stdin.take().unwrap();
            let reader = std::io::BufReader::new(parent.0.stdout.take().unwrap());
            let (sender, output) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.starts_with("TD-") && sender.send(line).is_err() {
                        break;
                    }
                }
            });
            Self {
                parent,
                input,
                output,
                pinned: Vec::new(),
            }
        }
        fn member(&mut self, linux: &mut Linux) -> u32 {
            let line = self.output.recv_timeout(Duration::from_secs(3)).unwrap();
            let pid = line.strip_prefix("TD-MEMBER ").unwrap().parse().unwrap();
            self.pinned
                .push(linux.reader.process_directory(pid).unwrap());
            pid
        }
        fn command(&mut self, command: &str) {
            writeln!(self.input, "{command}").unwrap();
            self.input.flush().unwrap();
        }
    }
    // Concurrent suites create and reap unrelated processes during the fresh scan.
    // Retry preparation only; an incomplete attempt has no send authority.
    fn prepare_family(
        linux: &mut Linux,
        request: Intent,
        active: &AtomicU64,
    ) -> (Prepared, Details) {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            match linux.prepare(request, active) {
                Ok(prepared) => return prepared,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        || error.to_string().contains("changed") =>
                {
                    assert!(
                        Instant::now() < until,
                        "fresh family never stabilized: {error}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("family preparation: {error}"),
            }
        }
    }
    #[test]
    fn captured_subtree_excludes_later_children_and_reports_partial_delivery() {
        let mut linux = setup();
        let mut family = Family::new();
        let first = family.member(&mut linux);
        let active = AtomicU64::new(1);
        let mut request = intent(&mut linux, family.parent.0.id(), 1, Signal::Stop);
        request.scope = Scope::Descendants;
        let (prepared, details) = prepare_family(&mut linux, request, &active);
        assert_eq!(details.rows.len(), 2);
        assert_eq!(
            prepared.targets.first().unwrap().observed.key.pid,
            family.parent.0.id()
        );
        assert_eq!(prepared.targets.get(1).unwrap().observed.key.pid, first);
        family.command("grow");
        let later = family.member(&mut linux);
        assert_eq!(
            &*linux.deliver(prepared, &active).deliveries,
            &[Delivery::Sent, Delivery::Sent]
        );
        wait_state(&mut linux, family.parent.0.id(), true);
        wait_state(&mut linux, first, true);
        wait_state(&mut linux, later, false);
        request.signal = Signal::Continue;
        let (prepared, details) = prepare_family(&mut linux, request, &active);
        assert_eq!(details.rows.len(), 3);
        assert_eq!(
            &*linux.deliver(prepared, &active).deliveries,
            &[Delivery::Sent; 3]
        );
        wait_state(&mut linux, family.parent.0.id(), false);
        wait_state(&mut linux, first, false);
        request.signal = Signal::Terminate;
        let (prepared, _) = prepare_family(&mut linux, request, &active);
        assert_eq!(
            prepared.targets.last().unwrap().observed.key.pid,
            family.parent.0.id()
        );
        let first_index = prepared
            .targets
            .iter()
            .position(|t| t.observed.key.pid == first)
            .unwrap();
        family.command("retire");
        assert_eq!(
            family.output.recv_timeout(Duration::from_secs(3)).unwrap(),
            "TD-RETIRED"
        );
        let results = linux.deliver(prepared, &active);
        assert_eq!(results.deliveries.len(), 3);
        assert_eq!(
            *results.deliveries.get(first_index).unwrap(),
            Delivery::Exited
        );
        assert_eq!(
            results
                .deliveries
                .iter()
                .filter(|r| **r == Delivery::Sent)
                .count(),
            2
        );
        wait_exit(&mut family.parent);
    }
}
