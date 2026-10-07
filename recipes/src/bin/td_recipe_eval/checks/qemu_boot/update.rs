//! Host-only release update oracle over a disposable overlay of an installation.
use super::{find_qemu, find_qemu_tool, qmp_json_path, Qmp};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use td_recipe::{ladder, td_boot_protocol};

use crate::atlas::{Slot, Style, PAGE_WIDTH};
use crate::face::Face;

type Result<T> = std::result::Result<T, String>;
const USAGE: &str = "usage: qemu-update --kernel FILE --selector FILE --disk FILE --format raw|qcow2 --work NEW-DIR [--timeout SECONDS] [--rollback yes|no] [--accel tcg|kvm]";
const READY_NOTICE: &str = " is ready. Press Ctrl+Alt+Escape, then I to review it.";
const LINE_LIMIT: usize = 1024 * 1024;
const LOG_LIMIT: u64 = 256 * 1024 * 1024;

struct Options {
    kernel: PathBuf,
    selector: PathBuf,
    disk: PathBuf,
    format: String,
    work: PathBuf,
    timeout: Duration,
    rollback: bool,
    accel: &'static str,
}

fn options(args: &[String]) -> Result<Options> {
    let mut values = std::collections::BTreeMap::new();
    for pair in args.chunks(2) {
        let [key, value] = pair else {
            return Err(USAGE.into());
        };
        if !matches!(
            key.as_str(),
            "--kernel"
                | "--selector"
                | "--disk"
                | "--format"
                | "--work"
                | "--timeout"
                | "--rollback"
                | "--accel"
        ) || values.insert(key.as_str(), value.as_str()).is_some()
        {
            return Err(USAGE.into());
        }
    }
    let get = |key| values.get(key).copied().ok_or_else(|| USAGE.to_string());
    let input = |key| -> Result<PathBuf> {
        let path = Path::new(get(key)?)
            .canonicalize()
            .map_err(|e| format!("resolve {key}: {e}"))?;
        if !fs::metadata(&path)
            .map_err(|e| format!("inspect {key}: {e}"))?
            .is_file()
        {
            return Err(format!("{key} must name a regular file"));
        }
        Ok(path)
    };
    let format = get("--format")?;
    if !matches!(format, "raw" | "qcow2") {
        return Err(USAGE.into());
    }
    let timeout = values
        .get("--timeout")
        .unwrap_or(&"21600")
        .parse::<u64>()
        .map_err(|_| USAGE.to_string())?;
    if !(60..=604800).contains(&timeout) {
        return Err(USAGE.into());
    }
    let rollback = match values.get("--rollback").copied().unwrap_or("no") {
        "yes" => true,
        "no" => false,
        _ => return Err(USAGE.into()),
    };
    let accel = match values.get("--accel").copied().unwrap_or("tcg") {
        "tcg" => "tcg",
        "kvm" => "kvm",
        _ => return Err(USAGE.into()),
    };
    Ok(Options {
        kernel: input("--kernel")?,
        selector: input("--selector")?,
        disk: input("--disk")?,
        format: format.into(),
        work: PathBuf::from(get("--work")?),
        timeout: Duration::from_secs(timeout),
        rollback,
        accel,
    })
}

fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))
}

struct Guest {
    child: Child,
    input: ChildStdin,
    lines: Option<Receiver<String>>,
    reader: Option<JoinHandle<Result<()>>>,
    qmp: Option<Qmp>,
    deadline: Instant,
    sequence: u64,
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.lines.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn serial_lines(
    mut output: impl Read,
    mut log: File,
    send: mpsc::SyncSender<String>,
) -> Result<()> {
    let mut buffer = [0; 8192];
    let mut line = Vec::new();
    let mut total = 0u64;
    loop {
        let n = output
            .read(&mut buffer)
            .map_err(|e| format!("read VM serial: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        let data = buffer.get(..n).ok_or("serial read exceeded buffer")?;
        total += n as u64;
        if total > LOG_LIMIT {
            return Err("VM serial log exceeded 256 MiB".into());
        }
        log.write_all(data)
            .map_err(|e| format!("save VM serial: {e}"))?;
        for byte in data {
            if *byte == b'\n' {
                let text = String::from_utf8_lossy(&line);
                // td-sh redraws its input with CR and ANSI sequences. Retain
                // the final nonempty segment, including ordinary CRLF output.
                let text = text
                    .rsplit('\r')
                    .find(|text| !text.is_empty())
                    .unwrap_or("");
                send.send(text.to_string())
                    .map_err(|e| format!("VM serial consumer closed: {e}"))?;
                line.clear();
            } else {
                if line.len() >= LINE_LIMIT {
                    return Err("VM serial line exceeded one MiB".into());
                }
                line.push(*byte);
            }
        }
    }
}

impl Guest {
    fn start(options: &Options, pass: u64, deadline: Instant) -> Result<Self> {
        Self::healthy(
            options,
            pass,
            deadline,
            &options.work.join("disk.qcow2"),
            |_| Ok(()),
        )
    }

    fn healthy(
        options: &Options,
        pass: u64,
        deadline: Instant,
        disk: &Path,
        mut observe: impl FnMut(&str) -> Result<()>,
    ) -> Result<Self> {
        let mut guest = Self::spawn(options, pass, deadline, disk, "")?;
        guest.until(Duration::from_secs(1800), |line| {
            observe(line)?;
            Ok((line == ladder::SYSTEM_BOOT_SUCCESS_MARKER).then_some(()))
        })?;
        guest.qmp = Some(Qmp::connect_until(
            &options.work.join(format!("qmp-{pass}.sock")),
            deadline.min(Instant::now() + Duration::from_secs(10)),
        )?);
        println!("[qemu-update] boot {pass} healthy");
        Ok(guest)
    }

    fn spawn(
        options: &Options,
        pass: u64,
        deadline: Instant,
        disk: &Path,
        extra: &str,
    ) -> Result<Self> {
        let socket = options.work.join(format!("qmp-{pass}.sock"));
        let log = private_file(&options.work.join(format!("serial-{pass}.log")))?;
        let mut error = private_file(&options.work.join(format!("qemu-{pass}.log")))?;
        writeln!(
            error,
            "[qemu-update] boot {pass} accelerator={}",
            options.accel
        )
        .map_err(|e| format!("record update VM accelerator: {e}"))?;
        let mut command = Command::new(find_qemu()?);
        command
            .args([
                "-M",
                "pc",
                "-accel",
                if options.accel == "tcg" {
                    "tcg,thread=multi"
                } else {
                    "kvm"
                },
                "-smp",
                "4",
                "-m",
                "12288",
                "-no-reboot",
                "-no-user-config",
                "-vga",
                "none",
                "-netdev",
                "user,id=net0",
                "-device",
                "virtio-net-pci,netdev=net0",
                "-device",
                "virtio-vga",
                "-audiodev",
                "none,id=audio0",
                "-device",
                "intel-hda",
                "-device",
                "hda-output,audiodev=audio0",
                "-device",
                "virtio-tablet-pci",
                "-display",
                "none",
                "-serial",
                "stdio",
                "-monitor",
                "none",
            ])
            .arg("-qmp")
            .arg(format!("unix:{},server=on,wait=off", socket.display()))
            .arg("-kernel")
            .arg(&options.kernel)
            .arg("-initrd")
            .arg(&options.selector)
            .arg("-append")
            .arg(format!("console=ttyS0 rdinit=/init {extra}"))
            .arg("-drive")
            .arg(format!(
                "if=none,format=qcow2,id=disk0,file={}",
                disk.display().to_string().replace(',', ",,")
            ))
            .args(["-device", "virtio-blk-pci,drive=disk0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(error);
        println!("[qemu-update] boot {pass} accelerator={}", options.accel);
        let mut child = command
            .spawn()
            .map_err(|e| format!("start update VM: {e}"))?;
        let pipes = (|| {
            let input = child.stdin.take().ok_or("VM has no serial input")?;
            let output = child.stdout.take().ok_or("VM has no serial output")?;
            Ok::<_, String>((input, output))
        })();
        let (input, output) = match pipes {
            Ok(parts) => parts,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let (send, lines) = mpsc::sync_channel(64);
        let reader = thread::spawn(move || serial_lines(output, log, send));
        let guest = Self {
            child,
            input,
            lines: Some(lines),
            reader: Some(reader),
            qmp: None,
            deadline,
            sequence: 0,
        };
        Ok(guest)
    }

    fn failed_boot(mut self, expected: &str, remaining: u8) -> Result<()> {
        let end = self
            .deadline
            .min(Instant::now() + Duration::from_secs(1800));
        let mut evidence = FailureEvidence::default();
        loop {
            let budget = end
                .checked_duration_since(Instant::now())
                .ok_or("failed boot did not shut down before its deadline")?;
            match self
                .lines
                .as_ref()
                .ok_or("VM serial receiver closed")?
                .recv_timeout(budget.min(Duration::from_secs(1)))
            {
                Ok(line) => evidence.observe(&line, expected, remaining)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.lines.take();
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_| "failed-boot serial reader failed")??;
        }
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| format!("wait for failed boot: {e}"))?
            {
                if !status.success() {
                    return Err(format!("failed boot did not exit cleanly: {status}"));
                }
                return evidence.finish();
            }
            if Instant::now() >= end {
                return Err("failed boot closed serial without exiting".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn until<T>(
        &mut self,
        budget: Duration,
        mut accept: impl FnMut(&str) -> Result<Option<T>>,
    ) -> Result<T> {
        let end = self.deadline.min(Instant::now() + budget);
        loop {
            let remaining = end
                .checked_duration_since(Instant::now())
                .ok_or("update VM phase timed out")?;
            match self
                .lines
                .as_ref()
                .ok_or("VM serial receiver closed")?
                .recv_timeout(remaining.min(Duration::from_secs(1)))
            {
                Ok(line) => {
                    if let Some(value) = accept(&line)? {
                        return Ok(value);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(reader) = self.reader.take() {
                        reader.join().map_err(|_| "VM serial reader failed")??;
                    }
                    return Err("VM serial ended before the required result".into());
                }
            }
        }
    }

    fn send(&mut self, command: &str) -> Result<()> {
        self.input
            .write_all(command.as_bytes())
            .and_then(|()| self.input.write_all(b"\n"))
            .and_then(|()| self.input.flush())
            .map_err(|e| format!("send VM command: {e}"))
    }

    fn command(&mut self, command: &str) -> Result<Vec<String>> {
        self.sequence += 1;
        let marker = format!("TD-UPDATE-ORACLE-STEP-{}=", self.sequence);
        self.send(&format!("{command}; echo {marker}$?"))?;
        let mut output = Vec::new();
        let mut bytes = 0usize;
        self.until(Duration::from_secs(600), |line| {
            if let Some(status) = line.strip_prefix(&marker) {
                if status != "0" {
                    return Err(format!("guest command refused ({status}): {command}"));
                }
                return Ok(Some(()));
            }
            bytes += line.len();
            if output.len() >= 4096 || bytes > 4 * 1024 * 1024 {
                return Err("guest command output exceeded 4096 lines or four MiB".into());
            }
            output.push(line.to_string());
            Ok(None)
        })?;
        Ok(output)
    }

    fn wait_for_source(&mut self) -> Result<()> {
        let original_deadline = self.deadline;
        self.deadline = original_deadline.min(Instant::now() + Duration::from_secs(600));
        let result: Result<()> = (|| loop {
            if Instant::now() >= self.deadline {
                return Err("release source initialization timed out".into());
            }
            let state = self.scalar(
                    "if test -L /var/home/tester/src/td/update; then echo source-ready; else echo source-pending; fi",
                    |line| matches!(line, "source-ready" | "source-pending"),
                )?;
            if state == "source-ready" {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(1));
        })();
        self.deadline = original_deadline;
        result.map_err(|error| format!("wait for release source initialization: {error}"))
    }

    fn scalar(&mut self, command: &str, valid: impl Fn(&str) -> bool) -> Result<String> {
        let mut output = self
            .command(command)?
            .into_iter()
            .filter(|line| valid(line));
        let result = output
            .next()
            .ok_or_else(|| format!("guest command returned no value: {command}"))?;
        if output.next().is_some() {
            return Err(format!(
                "guest command returned duplicate values: {command}"
            ));
        }
        Ok(result)
    }

    fn qmp(&mut self) -> Result<&mut Qmp> {
        self.qmp
            .as_mut()
            .ok_or_else(|| "VM monitor is not connected".into())
    }

    fn key(&mut self, keys: &[&str]) -> Result<()> {
        let end = self.deadline.min(Instant::now() + Duration::from_secs(10));
        self.qmp()?.key_chord_until(keys, end)
    }

    fn stop(mut self) -> Result<()> {
        let end = self.deadline.min(Instant::now() + Duration::from_secs(30));
        self.qmp()?.exchange_until(r#"{"execute":"quit"}"#, end)?;
        self.lines.take();
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| format!("wait for QEMU quit: {e}"))?
            {
                if !status.success() {
                    return Err(format!("QEMU quit failed: {status}"));
                }
                return Ok(());
            }
            if Instant::now() >= end {
                return Err("QEMU did not exit after quit".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn update_ready(&mut self, attempt: u64) -> Result<String> {
        self.send(&format!(
            "/bin/sh -c './update; echo TD-UPDATE-ORACLE-DONE-{attempt}=$?' &"
        ))?;
        let marker = format!("TD-UPDATE-ORACLE-DONE-{attempt}=");
        self.until(
            self.deadline.saturating_duration_since(Instant::now()),
            |line| {
                if line.starts_with(&marker) {
                    return Err(format!("update ended before installation request: {line}"));
                }
                Ok(ready_id(line).map(str::to_string))
            },
        )
    }

    fn update_done(&mut self, attempt: u64, success: bool) -> Result<()> {
        let marker = format!("TD-UPDATE-ORACLE-DONE-{attempt}=");
        self.until(Duration::from_secs(1800), |line| {
            let Some(status) = line.strip_prefix(&marker) else {
                return Ok(None);
            };
            let status = status
                .parse::<u8>()
                .map_err(|_| "malformed update exit status")?;
            if (status == 0) != success {
                return Err(format!("unexpected update result: {line}"));
            }
            Ok(Some(()))
        })
    }

    fn prompt(&mut self, id: &str, path: &Path) -> Result<()> {
        self.key(&["ctrl", "alt", "esc"])?;
        let end = self.deadline.min(Instant::now() + Duration::from_secs(20));
        self.qmp()?.move_absolute_until(0, 0, end)?;
        let filename = qmp_json_path(path)?;
        loop {
            self.qmp()?
                .exchange_until(
                    &format!(
                        "{{\"execute\":\"screendump\",\"arguments\":{{\"filename\":{filename}}}}}"
                    ),
                    end,
                )
                .map_err(|error| format!("capture secure-attention menu: {error}"))?;
            let bytes = read_capture(path)?;
            if menu_matches(&bytes)? {
                break;
            }
            if Instant::now() >= end {
                return Err("secure-attention menu did not appear".into());
            }
            thread::sleep(Duration::from_millis(100));
        }
        self.key(&["i"])?;
        let end = self.deadline.min(Instant::now() + Duration::from_secs(20));
        loop {
            self.qmp()?
                .exchange_until(
                    &format!(
                        "{{\"execute\":\"screendump\",\"arguments\":{{\"filename\":{filename}}}}}"
                    ),
                    end,
                )
                .map_err(|error| format!("capture installation prompt: {error}"))?;
            if prompt_matches(&read_capture(path)?, id)? {
                return Ok(());
            }
            if Instant::now() >= end {
                return Err(
                    "trusted prompt pixels did not match the full expected installation".into(),
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn git_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64)
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_id(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn kernel_id(line: &str) -> Option<&str> {
    let mut ids = line
        .split_ascii_whitespace()
        .filter_map(|word| word.strip_prefix("td.deployment="));
    let id = ids.next()?;
    (canonical_id(id) && ids.next().is_none()).then_some(id)
}

fn ready_id(line: &str) -> Option<&str> {
    line.strip_prefix("Update ")
        .and_then(|line| line.strip_suffix(READY_NOTICE))
        .filter(|id| canonical_id(id))
}

fn selector(guest: &mut Guest, slot: &str) -> Result<String> {
    guest
        .scalar(&format!("readlink /run/td-volume/td/boot/{slot}"), |line| {
            line.strip_prefix("../deployments/")
                .is_some_and(canonical_id)
        })
        .map(|line| line.trim_start_matches("../deployments/").to_string())
}

fn once(flag: &mut bool, matches: bool, action: &str) -> Result<()> {
    if *flag || !matches {
        return Err(format!("unexpected or duplicate {action}"));
    }
    *flag = true;
    Ok(())
}

#[derive(Default)]
struct FailureEvidence {
    selected: bool,
    consumed: bool,
    markers: std::collections::BTreeSet<&'static str>,
}

const FAILURE_MARKERS: &[&str] = &[
    ladder::SYSTEM_ROOT_RO_MARKER,
    ladder::SYSTEM_ETC_RO_MARKER,
    ladder::SYSTEM_STATE_WRITABLE_MARKER,
    ladder::SYSTEM_STATE_OWNER_MARKER,
    ladder::SYSTEM_SHUTDOWN_MARKER,
];

impl FailureEvidence {
    fn observe(&mut self, line: &str, id: &str, remaining: u8) -> Result<()> {
        if line.starts_with("TD-BOOT-SELECTED-") {
            once(
                &mut self.selected,
                line == format!("{} {id}", td_boot_protocol::SELECTED_CURRENT_MARKER),
                "failed-boot selection",
            )?;
        }
        if line.starts_with(td_boot_protocol::ATTEMPT_CONSUMED_MARKER) {
            once(
                &mut self.consumed,
                line == format!(
                    "{} {id} remaining={remaining}",
                    td_boot_protocol::ATTEMPT_CONSUMED_MARKER
                ),
                "failed-boot attempt",
            )?;
        }
        if line.starts_with(td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER)
            || line == ladder::SYSTEM_BOOT_SUCCESS_MARKER
            || line == ladder::GREETER_MARKER
            || line.contains("Kernel panic")
        {
            return Err(format!(
                "failed candidate unexpectedly reached health or fallback: {line}"
            ));
        }
        for &marker in FAILURE_MARKERS {
            if line == marker {
                self.markers.insert(marker);
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        if !self.selected || !self.consumed || self.markers.len() != FAILURE_MARKERS.len() {
            return Err(
                "failed boot lacks selection, durable attempt, root/state or shutdown evidence"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Default)]
struct RollbackEvidence {
    selected: bool,
    exhausted: bool,
}

impl RollbackEvidence {
    fn observe(&mut self, line: &str, failed: &str, previous: &str, first: bool) -> Result<()> {
        if line.starts_with("TD-BOOT-SELECTED-") {
            let marker = if first {
                td_boot_protocol::SELECTED_PREVIOUS_MARKER
            } else {
                td_boot_protocol::SELECTED_CURRENT_MARKER
            };
            once(
                &mut self.selected,
                line == format!("{marker} {previous}"),
                "rollback selection",
            )?;
        }
        if line.starts_with(td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER) {
            once(
                &mut self.exhausted,
                first
                    && line
                        == format!(
                            "{} {failed} -> {previous}",
                            td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER
                        ),
                "attempt exhaustion",
            )?;
        }
        if line.starts_with(td_boot_protocol::ATTEMPT_CONSUMED_MARKER)
            || line.contains("Kernel panic")
        {
            return Err(format!(
                "rollback boot consumed an attempt or panicked: {line}"
            ));
        }
        Ok(())
    }

    fn finish(&self, first: bool) -> Result<()> {
        if !self.selected || self.exhausted != first {
            return Err("rollback selection evidence is incomplete".into());
        }
        Ok(())
    }
}

fn overlay(backing: &Path, format: &str, disk: &Path) -> Result<()> {
    let image = find_qemu_tool("qemu-img").ok_or("qemu-img is required")?;
    let status = Command::new(image)
        .args(["create", "-f", "qcow2", "-F", format, "-b"])
        .arg(backing)
        .arg(disk)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("create test overlay: {e}"))?;
    if !status.success() {
        return Err(format!("qemu-img create failed: {status}"));
    }
    Ok(())
}

fn verify_persistence(
    guest: &mut Guest,
    expected: &str,
    public: &str,
    source: &str,
    token: &str,
) -> Result<()> {
    let cmdline = guest.scalar("cat /proc/cmdline", |line| kernel_id(line).is_some())?;
    if kernel_id(&cmdline) != Some(expected) {
        return Err("reboot selected a different deployment".into());
    }
    guest.command(&format!("test \"$(cat /var/home/tester/{token})\" = {token} && test ! -r /var/lib/td-deploy/deployment.pk8"))?;
    if guest.scalar("cat /run/td-volume/td/trusted.pub", canonical_id)? != public {
        return Err("installation changed its public signing identity".into());
    }
    guest.command("cd /var/home/tester/src/td")?;
    if guest.scalar("git hash-object td-update/src/main.rs", git_id)? != source {
        return Err("reboot changed the user's source edit".into());
    }
    Ok(())
}

fn rollback(
    options: &Options,
    deadline: Instant,
    initial: &str,
    successor: &str,
    public: &str,
    source: &str,
    token: &str,
) -> Result<()> {
    let disk = options.work.join("rollback.qcow2");
    for attempt in 0..td_boot_protocol::DEFAULT_BOOT_ATTEMPTS {
        let pass = u64::from(attempt) + 3;
        let guest = Guest::spawn(
            options,
            pass,
            deadline,
            &disk,
            ladder::BOOT_FAIL_TARGET_CMDLINE_TOKEN,
        )?;
        guest.failed_boot(
            successor,
            td_boot_protocol::DEFAULT_BOOT_ATTEMPTS - attempt - 1,
        )?;
        println!(
            "[qemu-update] failed successor boot {} consumed its durable attempt",
            attempt + 1
        );
    }
    for (offset, first) in [true, false].into_iter().enumerate() {
        let pass = u64::from(td_boot_protocol::DEFAULT_BOOT_ATTEMPTS) + 3 + offset as u64;
        let mut evidence = RollbackEvidence::default();
        let mut guest = Guest::healthy(options, pass, deadline, &disk, |line| {
            evidence.observe(line, successor, initial, first)
        })?;
        evidence.finish(first)?;
        verify_persistence(&mut guest, initial, public, source, token)?;
        if selector(&mut guest, "current")? != initial {
            return Err("automatic rollback did not persist current".into());
        }
        guest.command("sync")?;
        guest.stop()?;
    }
    println!("[qemu-update] automatic rollback and its next boot preserved user state and signing identity");
    Ok(())
}

pub(crate) fn run_cli(args: &[String]) -> Result<()> {
    let mut options = options(args)?;
    let name = options
        .work
        .file_name()
        .ok_or("oracle work directory has no final name")?;
    let parent = options
        .work
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = parent
        .canonicalize()
        .map_err(|e| format!("resolve oracle parent: {e}"))?;
    options.work = parent.join(name);
    if options.work.as_os_str().as_encoded_bytes().len() > 70
        || options.work.to_string_lossy().contains(',')
    {
        return Err(
            "oracle directory must fit a Unix socket path (at most 70 bytes) and contain no comma"
                .into(),
        );
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&options.work)
        .map_err(|e| format!("create new oracle directory: {e}"))?;
    overlay(
        &options.disk,
        &options.format,
        &options.work.join("disk.qcow2"),
    )?;
    let deadline = Instant::now()
        .checked_add(options.timeout)
        .ok_or("oracle deadline overflow")?;
    let mut nonce = [0u8; 8];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut nonce))
        .map_err(|e| format!("read fixture nonce: {e}"))?;
    let nonce = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let token = format!("td-update-oracle-{nonce}");
    let mut guest = Guest::start(&options, 1, deadline)?;
    println!("[qemu-update] waiting for the guest source checkout");
    let source_wait = Instant::now();
    guest.wait_for_source()?;
    println!(
        "[qemu-update] guest source checkout ready after {}s",
        source_wait.elapsed().as_secs()
    );
    guest.command("cd /var/home/tester/src/td && test -L update && test ! -r /var/lib/td-deploy/deployment.pk8")?;
    let initial = selector(&mut guest, "current")?;
    let previous = selector(&mut guest, "previous")?;
    let public = guest.scalar("cat /run/td-volume/td/trusted.pub", canonical_id)?;
    // Only the disposable overlay's checkout is edited; the input disk is a
    // read-only backing file. A unique HELP marker distinguishes a real build.
    guest.command(&format!("test \"$(grep -c '^const HELP: ' td-update/src/main.rs)\" = 1 && sed '/^const HELP: /s/usage: td-update/usage: {token}/' td-update/src/main.rs > td-update/src/main.rs.oracle && mv td-update/src/main.rs.oracle td-update/src/main.rs"))?;
    guest.command(&format!(
        "printf '%s\\n' {token} > /var/home/tester/{token}"
    ))?;
    let source = guest.scalar("git hash-object td-update/src/main.rs", git_id)?;
    println!("[qemu-update] building successor from the guest checkout");
    let successor = guest.update_ready(1)?;
    if initial == successor {
        return Err("native update did not produce a distinct deployment".into());
    }
    guest.prompt(&successor, &options.work.join("cancel.ppm"))?;
    guest.key(&["esc"])?;
    guest.update_done(1, false)?;
    if selector(&mut guest, "current")? != initial || selector(&mut guest, "previous")? != previous
    {
        return Err("cancelled update changed the deployment selectors".into());
    }
    println!("[qemu-update] cancellation preserved both selectors; requesting installation");
    if guest.update_ready(2)? != successor {
        return Err("unchanged checkout produced a different deployment".into());
    }
    guest.prompt(&successor, &options.work.join("install.ppm"))?;
    guest.key(&["ret"])?;
    guest.update_done(2, true)?;
    if selector(&mut guest, "current")? != successor || selector(&mut guest, "previous")? != initial
    {
        return Err("installation did not publish the expected current/previous pair".into());
    }
    guest.command("sync")?;
    guest.stop()?;
    if options.rollback {
        let pending = options.work.join("pending.qcow2");
        fs::rename(options.work.join("disk.qcow2"), &pending)
            .map_err(|e| format!("preserve the stopped pending installation: {e}"))?;
        overlay(&pending, "qcow2", &options.work.join("disk.qcow2"))?;
        overlay(&pending, "qcow2", &options.work.join("rollback.qcow2"))?;
    }
    let mut guest = Guest::start(&options, 2, deadline)?;
    verify_persistence(&mut guest, &successor, &public, &source, &token)?;
    guest.command(&format!("/bin/td-update --help | grep -q {token}"))?;
    guest.command("sync")?;
    guest.stop()?;
    if options.rollback {
        rollback(
            &options, deadline, &initial, &successor, &public, &source, &token,
        )?;
    }
    let report = format!("initial={initial}\nsuccessor={successor}\npublic={public}\nsource={source}\ncancel_preserved_selectors=true\nphysical_prompt_verified=true\ninstalled_and_booted=true\nuser_data_preserved=true\nautomatic_rollback_verified={}\naccelerator={}\n", options.rollback, options.accel);
    private_file(&options.work.join("result.txt"))?
        .write_all(report.as_bytes())
        .map_err(|e| format!("write oracle result: {e}"))?;
    println!("[qemu-update] PASS: native build, cancellation, confirmed installation, reboot and persistence");
    Ok(())
}

// Printable ASCII from the compositor's pinned Unifont PSF2 face.
// See td-compositor/assets/PROVENANCE. The test below binds these pixels
// and their Unicode mapping to that face.
const ASCII_HEX: &str = "000000000000000000000000000000000000000008080808080808000808000000002222222200000000000000000000000000001212127e24247e484848000000000000083e4948380e09493e08000000000000314a4a340808162929460000000000001c222214182945424639000000000808080800000000000000000000000000040808101010101010080804000000002010100808080808081010200000000000000008492a1c2a49080000000000000000000808087f080808000000000000000000000000000000180808100000000000000000003c000000000000000000000000000000000000181800000000000002020408081010204040000000000000182442464a52624224180000000000000818280808080808083e0000000000003c4242020c102040407e0000000000003c4242021c020242423c000000000000040c142444447e0404040000000000007e4040407c020202423c0000000000001c2040407c424242423c0000000000007e0202040404080808080000000000003c4242423c424242423c0000000000003c4242423e02020204380000000000000000181800000018180000000000000000001818000000180808100000000000000204081020100804020000000000000000007e0000007e0000000000000000004020100804081020400000000000003c4242020408080008080000000000001c224a565252524e201e00000000000018242442427e424242420000000000007c4242427c424242427c0000000000003c42424040404042423c000000000000784442424242424244780000000000007e4040407c404040407e0000000000007e4040407c40404040400000000000003c424240404e4242463a000000000000424242427e42424242420000000000003e08080808080808083e0000000000001f040404040404444438000000000000424448506060504844420000000000004040404040404040407e000000000000424266665a5a4242424200000000000042626252524a4a4646420000000000003c42424242424242423c0000000000007c4242427c40404040400000000000003c4242424242425a663c0300000000007c4242427c48444442420000000000003c424240300c0242423c0000000000007f0808080808080808080000000000004242424242424242423c00000000000041414122222214140808000000000000424242425a5a6666424200000000000042422424181824244242000000000000414122221408080808080000000000007e02020408102040407e00000000000e080808080808080808080e0000000000404020101008080402020000000000701010101010101010101070000000182442000000000000000000000000000000000000000000000000007f00002010080000000000000000000000000000000000003c42023e4242463a00000000004040405c6242424242625c00000000000000003c4240404040423c00000000000202023a4642424242463a00000000000000003c42427e4040423c00000000000c1010107c10101010101000000000000000023a44444438203c42423c0000004040405c624242424242420000000000080800180808080808083e00000000000404000c04040404040404483000000040404044485060504844420000000000180808080808080808083e0000000000000000764949494949494900000000000000005c6242424242424200000000000000003c4242424242423c00000000000000005c6242424242625c40400000000000003a4642424242463a02020000000000005c6242404040404000000000000000003c4240300c02423c0000000000001010107c10101010100c0000000000000000424242424242463a00000000000000004242422424241818000000000000000041494949494949360000000000000000424224181824424200000000000000004242424242261a02023c0000000000007e0204081020407e00000000000c10100808102010080810100c000008080808080808080808080808080000003008081010080408101008083000000031494600000000000000000000";

pub(super) fn read_capture(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(1280 * 800 * 3 + 128).read_to_end(&mut bytes))
        .map_err(|e| format!("read display capture {}: {e}", path.display()))?;
    Ok(bytes)
}

pub(super) fn ppm(bytes: &[u8]) -> Result<&[u8]> {
    let body = bytes
        .strip_prefix(b"P6\n1280 800\n255\n")
        .ok_or("expected a 1280x800 RGB QMP screenshot")?;
    if body.len() != 1280 * 800 * 3 {
        return Err("truncated or oversized QMP screenshot".into());
    }
    Ok(body)
}

pub(super) fn ascii_pixel(character: u8, x: usize, y: usize) -> Result<bool> {
    if !(32..=126).contains(&character) || x >= 8 || y >= 16 {
        return Err("unsupported prompt glyph".into());
    }
    let offset = (usize::from(character - 32) * 16 + y) * 2;
    let text = ASCII_HEX
        .get(offset..offset + 2)
        .ok_or("missing ASCII bitmap")?;
    let row = u8::from_str_radix(text, 16).map_err(|_| "invalid ASCII bitmap")?;
    Ok(row & (0x80 >> x) != 0)
}

pub(super) fn row_matches(pixels: &[u8], top: usize, text: &str) -> Result<bool> {
    if text.len() > 77 || top + 32 > 800 {
        return Err("prompt row does not fit the display".into());
    }
    for y in 0..32 {
        for x in 0usize..1280 {
            let on = if let Some(column) = x.checked_sub(24) {
                match text.as_bytes().get(column / 16) {
                    Some(character) => ascii_pixel(*character, column % 16 / 2, y / 2)?,
                    None => false,
                }
            } else {
                false
            };
            let expected: &[u8] = if on { &[255, 255, 255] } else { &[24, 32, 40] };
            let offset = ((top + y) * 1280 + x) * 3;
            if pixels.get(offset..offset + 3) != Some(expected) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

// The attention menu uses the small chrome face; consent uses Unifont.
// Rows are space, colon, A through Z, then hyphen and plus.
const MENU_GLYPHS: [[u8; 7]; 30] = [
    [0, 0, 0, 0, 0, 0, 0],
    [0, 4, 4, 0, 4, 4, 0],
    [14, 17, 17, 31, 17, 17, 17],
    [30, 17, 17, 30, 17, 17, 30],
    [14, 17, 16, 16, 16, 17, 14],
    [30, 17, 17, 17, 17, 17, 30],
    [31, 16, 16, 30, 16, 16, 31],
    [31, 16, 16, 30, 16, 16, 16],
    [14, 17, 16, 23, 17, 17, 15],
    [17, 17, 17, 31, 17, 17, 17],
    [14, 4, 4, 4, 4, 4, 14],
    [7, 2, 2, 2, 18, 18, 12],
    [17, 18, 20, 24, 20, 18, 17],
    [16, 16, 16, 16, 16, 16, 31],
    [17, 27, 21, 21, 17, 17, 17],
    [17, 25, 21, 19, 17, 17, 17],
    [14, 17, 17, 17, 17, 17, 14],
    [30, 17, 17, 30, 16, 16, 16],
    [14, 17, 17, 17, 21, 18, 13],
    [30, 17, 17, 30, 20, 18, 17],
    [15, 16, 16, 14, 1, 1, 30],
    [31, 4, 4, 4, 4, 4, 4],
    [17, 17, 17, 17, 17, 17, 14],
    [17, 17, 17, 17, 17, 10, 4],
    [17, 17, 17, 21, 21, 21, 10],
    [17, 17, 10, 4, 10, 17, 17],
    [17, 17, 10, 4, 4, 4, 4],
    [31, 1, 2, 4, 8, 16, 31],
    [0, 0, 0, 31, 0, 0, 0],
    [0, 4, 4, 31, 4, 4, 0],
];

/// The chrome face's rows for `character`, top first, five bits each.
fn chrome_glyph(character: u8) -> Result<&'static [u8; 7]> {
    let index = match character {
        b' ' => 0,
        b':' => 1,
        b'A'..=b'Z' => usize::from(character - b'A') + 2,
        b'-' => 28,
        b'+' => 29,
        _ => return Err("unsupported chrome glyph".into()),
    };
    MENU_GLYPHS
        .get(index)
        .ok_or_else(|| "missing chrome glyph".to_string())
}

pub(super) fn menu_row_matches(pixels: &[u8], top: usize, text: &str) -> Result<bool> {
    if text.len() > 102 || top > 800 - 14 {
        return Err("menu row does not fit the display".into());
    }
    for y in 0..14 {
        for x in 0usize..1280 {
            let on = if let Some(column) = x.checked_sub(24) {
                match text.as_bytes().get(column / 12) {
                    Some(character) if column % 12 < 10 => {
                        let bits = chrome_glyph(*character)?
                            .get(y / 2)
                            .ok_or("missing menu glyph row")?;
                        bits & (1 << (4 - column % 12 / 2)) != 0
                    }
                    _ => false,
                }
            } else {
                false
            };
            let expected: &[u8] = if on { &[255, 255, 255] } else { &[24, 32, 40] };
            let offset = ((top + y) * 1280 + x) * 3;
            if pixels.get(offset..offset + 3) != Some(expected) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// The status bar's band and colours (td-compositor/src/bar.rs) and its text
/// (td-compositor/src/text.rs): one row of 8x16 cells 4 pixels down a 24-pixel
/// band, each character the pinned outline face fitted to its cell and
/// clipped to it, coverage blended from the background toward the ink, and a
/// character the face lacks Unifont's cell in ink. The colours are the
/// compositor's BGRX constants as they scan out.
const BAR_HEIGHT: usize = 24;
const BAR_TEXT_TOP: usize = 4;
const BAR_CELL_WIDTH: usize = 8;
const BAR_CELL_HEIGHT: usize = 16;
const BAR_INK: [u8; 3] = [0xe0, 0xc8, 0xd0];
const BAR_BACKGROUND: [u8; 3] = [0x20, 0x14, 0x18];

/// A suffix of the status bar's line as the compositor draws it: the band's
/// rows across `width` columns, and the last column anything marks.
pub(super) struct BarText {
    width: usize,
    pixels: Vec<[u8; 3]>,
    last: usize,
}

impl BarText {
    /// `text` drawn as the bar draws it, in the face whose regular style is
    /// `regular`.
    pub(super) fn render(regular: Vec<u8>, text: &str) -> Result<Self> {
        let mut face = Face::fit(regular.into(), None, BAR_CELL_WIDTH, BAR_CELL_HEIGHT)
            .map_err(|error| format!("bar face: {error}"))?;
        let unifont = crate::font::pinned()?;
        let width = text
            .chars()
            .count()
            .checked_mul(BAR_CELL_WIDTH)
            .ok_or("bar text is too long")?;
        let mut pixels = vec![BAR_BACKGROUND; width * BAR_HEIGHT];
        for (column, character) in text.chars().enumerate() {
            let left = column * BAR_CELL_WIDTH;
            let cell = face.cell();
            // Each glyph is clipped to its own cell, as the compositor clips.
            let mut put = |x: i64, y: i64, color: [u8; 3]| {
                let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
                    return;
                };
                if (left..left + BAR_CELL_WIDTH).contains(&x)
                    && (BAR_TEXT_TOP..BAR_TEXT_TOP + BAR_CELL_HEIGHT).contains(&y)
                {
                    if let Some(pixel) = pixels.get_mut(y * width + x) {
                        *pixel = color;
                    }
                }
            };
            match face.glyph(Style::Regular, character) {
                Slot::Blank => {}
                Slot::Placed(entry) => {
                    let page = face.atlas().page();
                    let pen = i64::try_from(left + cell.pen).map_err(|_| "bar pen")?;
                    let baseline =
                        i64::try_from(BAR_TEXT_TOP + cell.baseline).map_err(|_| "bar baseline")?;
                    for row in 0..entry.height {
                        for col in 0..entry.width {
                            let alpha = page
                                .get((entry.y + row) * PAGE_WIDTH + entry.x + col)
                                .copied()
                                .unwrap_or(0);
                            if alpha == 0 {
                                continue;
                            }
                            let x = pen + i64::from(entry.left) + i64::try_from(col).unwrap_or(0);
                            let y =
                                baseline - i64::from(entry.top) + i64::try_from(row).unwrap_or(0);
                            put(x, y, mix(BAR_BACKGROUND, BAR_INK, alpha));
                        }
                    }
                }
                Slot::Missing => {
                    let index = unifont.index(character);
                    for row in 0..unifont.height().min(BAR_CELL_HEIGHT) {
                        for col in 0..unifont.width().min(BAR_CELL_WIDTH) {
                            if unifont.pixel(index, col, row) {
                                let x = i64::try_from(left + col).map_err(|_| "bar column")?;
                                let y = i64::try_from(BAR_TEXT_TOP + row).map_err(|_| "bar row")?;
                                put(x, y, BAR_INK);
                            }
                        }
                    }
                }
            }
        }
        let last = (0..width)
            .rev()
            .find(|&x| (0..BAR_HEIGHT).any(|y| pixels.get(y * width + x) != Some(&BAR_BACKGROUND)))
            .ok_or("the bar text marks nothing")?;
        Ok(Self {
            width,
            pixels,
            last,
        })
    }

    /// Whether the status bar's line in a 1280x800 RGB capture ends with
    /// this text: the bar's last mark is the text's last, and every pixel
    /// from the text's first cell to the screen's right edge, across the
    /// band, is exactly as drawn here, with nothing after it.
    pub(super) fn ends(&self, pixels: &[u8]) -> Result<bool> {
        let pixel = |x: usize, y: usize| pixels.get((y * 1280 + x) * 3..(y * 1280 + x) * 3 + 3);
        if pixels.len() != 1280 * 800 * 3 {
            return Err("expected a 1280x800 RGB capture".into());
        }
        let Some(end) = (0..1280usize)
            .rev()
            .find(|&x| (0..BAR_HEIGHT).any(|y| pixel(x, y) != Some(&BAR_BACKGROUND[..])))
        else {
            return Ok(false);
        };
        let Some(start) = end.checked_sub(self.last) else {
            return Ok(false);
        };
        for y in 0..BAR_HEIGHT {
            for x in start..1280 {
                let column = x - start;
                let expected = if column < self.width {
                    self.pixels
                        .get(y * self.width + column)
                        .ok_or("bar text pixel out of range")?
                } else {
                    &BAR_BACKGROUND
                };
                if pixel(x, y) != Some(&expected[..]) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

/// `from` moved toward `to` by `alpha` of 255 in each channel, rounded, as
/// the compositor's chrome text blends.
pub(super) fn mix(from: [u8; 3], to: [u8; 3], alpha: u8) -> [u8; 3] {
    let alpha = u16::from(alpha);
    let mut color = [0; 3];
    for ((out, from), to) in color.iter_mut().zip(from).zip(to) {
        let mixed = (u16::from(from) * (255 - alpha) + u16::from(to) * alpha + 127) / 255;
        *out = u8::try_from(mixed).unwrap_or(u8::MAX);
    }
    color
}

fn menu_matches(bytes: &[u8]) -> Result<bool> {
    menu_pixels_match(ppm(bytes)?)
}

/// Whether a capture's pixels are the attention menu offering a pending
/// installation.
pub(super) fn menu_pixels_match(pixels: &[u8]) -> Result<bool> {
    Ok(menu_row_matches(pixels, 276, "TD SECURE ATTENTION")?
        && menu_row_matches(pixels, 456, "I: REVIEW PENDING SYSTEM INSTALLATION")?)
}

fn prompt_matches(bytes: &[u8], id: &str) -> Result<bool> {
    prompt_pixels_match(ppm(bytes)?, id)
}

/// Whether a capture's pixels are the trusted installation prompt for
/// deployment `id`.
pub(super) fn prompt_pixels_match(pixels: &[u8], id: &str) -> Result<bool> {
    if !canonical_id(id) {
        return Err("invalid expected deployment ID".into());
    }
    // Eight rows including the variable countdown, 32px glyphs + 8px gaps.
    // Check every static row independently of the compositor's text builder.
    for (row, text) in [
        "TD SECURE ATTENTION",
        "SESSION USER 1000",
        "INSTALL BUILT SYSTEM",
        &format!("DEPLOYMENT: {id}"),
        "PREVIOUS SYSTEM KEPT FOR ROLLBACK",
        "RESTART REQUIRED TO USE THIS SYSTEM",
        "ENTER: INSTALL   ESC: CANCEL",
    ]
    .iter()
    .enumerate()
    {
        if !row_matches(pixels, 244 + row * 40, text)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn failure_lines(id: &str) -> Vec<String> {
        let mut lines: Vec<String> = FAILURE_MARKERS.iter().map(|m| m.to_string()).collect();
        lines.push(format!(
            "{} {id}",
            td_boot_protocol::SELECTED_CURRENT_MARKER
        ));
        lines.push(format!(
            "{} {id} remaining=1",
            td_boot_protocol::ATTEMPT_CONSUMED_MARKER
        ));
        lines
    }

    #[test]
    fn failed_boot_requires_complete_evidence_and_refuses_success_or_wrong_attempts() {
        let id = "a".repeat(64);
        let lines = failure_lines(&id);
        let mut good = FailureEvidence::default();
        for line in &lines {
            good.observe(line, &id, 1).unwrap();
        }
        good.finish().unwrap();
        for omitted in 0..lines.len() {
            let mut evidence = FailureEvidence::default();
            for (index, line) in lines.iter().enumerate() {
                if index != omitted {
                    evidence.observe(line, &id, 1).unwrap();
                }
            }
            assert!(evidence.finish().is_err());
        }
        for line in [
            ladder::SYSTEM_BOOT_SUCCESS_MARKER.to_string(),
            "TD-GREETER-OK".into(),
            "Kernel panic - not syncing".into(),
            format!(
                "{} {id} remaining=2",
                td_boot_protocol::ATTEMPT_CONSUMED_MARKER
            ),
            format!(
                "{} {}",
                td_boot_protocol::SELECTED_CURRENT_MARKER,
                "b".repeat(64)
            ),
            format!("{} {id}", td_boot_protocol::SELECTED_PREVIOUS_MARKER),
            td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER.into(),
        ] {
            assert!(
                FailureEvidence::default().observe(&line, &id, 1).is_err(),
                "{line}"
            );
        }
        assert!(good
            .observe(
                &format!(
                    "{} {id} remaining=1",
                    td_boot_protocol::ATTEMPT_CONSUMED_MARKER
                ),
                &id,
                1
            )
            .is_err());
    }

    #[test]
    fn rollback_requires_exact_exhaustion_then_attempt_free_persisted_current() {
        let failed = "a".repeat(64);
        let previous = "b".repeat(64);
        for first in [true, false] {
            let mut evidence = RollbackEvidence::default();
            assert!(evidence.finish(first).is_err());
            let marker = if first {
                td_boot_protocol::SELECTED_PREVIOUS_MARKER
            } else {
                td_boot_protocol::SELECTED_CURRENT_MARKER
            };
            evidence
                .observe(&format!("{marker} {previous}"), &failed, &previous, first)
                .unwrap();
            if first {
                assert!(evidence.finish(first).is_err());
                evidence
                    .observe(
                        &format!(
                            "{} {failed} -> {previous}",
                            td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER
                        ),
                        &failed,
                        &previous,
                        first,
                    )
                    .unwrap();
            }
            evidence.finish(first).unwrap();
            assert!(evidence
                .observe(&format!("{marker} {previous}"), &failed, &previous, first)
                .is_err());
            assert!(RollbackEvidence::default()
                .observe(&format!("{marker} {failed}"), &failed, &previous, first)
                .is_err());
            assert!(RollbackEvidence::default()
                .observe(
                    td_boot_protocol::ATTEMPT_CONSUMED_MARKER,
                    &failed,
                    &previous,
                    first
                )
                .is_err());
        }
        assert!(RollbackEvidence::default()
            .observe(
                &format!(
                    "{} {failed} -> {previous}",
                    td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER
                ),
                &failed,
                &previous,
                false
            )
            .is_err());
    }

    #[test]
    fn ascii_oracle_matches_the_pinned_face_and_unicode_table() {
        let source = include_str!("../../../../../../td-compositor/src/font_data.rs");
        let face = source
            .lines()
            .find_map(|line| {
                line.strip_prefix("pub const UNIFONT_HEX: &str = \"")?
                    .strip_suffix("\";")
            })
            .unwrap();
        assert!(
            face.starts_with("72b54a86000000002000000001000000c1500000100000001000000008000000")
        );
        assert_eq!(face.get(64..64 + 95 * 32), Some(ASCII_HEX));
        let unicode = (32 + 20673 * 16) * 2;
        let expected = (32..=126)
            .map(|code| format!("{code:02x}ff"))
            .collect::<String>();
        assert_eq!(
            face.get(unicode..unicode + expected.len()),
            Some(expected.as_str())
        );
    }

    fn screenshot(id: &str) -> Vec<u8> {
        let mut pixels = [24, 32, 40].repeat(1280 * 800);
        for (row, text) in [
            "TD SECURE ATTENTION",
            "SESSION USER 1000",
            "INSTALL BUILT SYSTEM",
            &format!("DEPLOYMENT: {id}"),
            "PREVIOUS SYSTEM KEPT FOR ROLLBACK",
            "RESTART REQUIRED TO USE THIS SYSTEM",
            "ENTER: INSTALL   ESC: CANCEL",
        ]
        .iter()
        .enumerate()
        {
            for (column, character) in text.bytes().enumerate() {
                for y in 0..32 {
                    for x in 0..16 {
                        if ascii_pixel(character, x / 2, y / 2).unwrap() {
                            let offset = ((244 + row * 40 + y) * 1280 + 24 + column * 16 + x) * 3;
                            pixels[offset..offset + 3].copy_from_slice(&[255; 3]);
                        }
                    }
                }
            }
        }
        let mut bytes = b"P6\n1280 800\n255\n".to_vec();
        bytes.extend(pixels);
        bytes
    }

    #[test]
    fn secure_attention_menu_uses_its_captured_small_chrome_font() {
        let chrome = include_str!("../../../../../../td-compositor/src/ui.rs");
        let mut pixels = [24, 32, 40].repeat(1280 * 800);
        // These complete row hashes came from the failed native menu capture.
        // The rest of that final capture was truncated at its QMP deadline.
        for (top, text, captured) in [
            (
                276,
                "TD SECURE ATTENTION",
                "397854906d2ca6aee82037ec663f1339a0a2b29569e813eab7252652b6063bed",
            ),
            (
                456,
                "I: REVIEW PENDING SYSTEM INSTALLATION",
                "aa378e2216f5d66058da6da5b203f0e4c3da804f3bd82dec032dbe542ab9ecf9",
            ),
        ] {
            for (column, character) in text.chars().enumerate() {
                let prefix = format!("b'{character}' => [");
                let rows = chrome
                    .lines()
                    .find_map(|line| line.trim().strip_prefix(&prefix)?.strip_suffix("],"))
                    .unwrap();
                for (y, bits) in rows
                    .split(',')
                    .map(|row| row.trim().parse::<u8>().unwrap())
                    .enumerate()
                {
                    for x in 0..5 {
                        if bits & (1 << (4 - x)) == 0 {
                            continue;
                        }
                        for dy in 0..2 {
                            for dx in 0..2 {
                                let offset =
                                    ((top + y * 2 + dy) * 1280 + 24 + column * 12 + x * 2 + dx) * 3;
                                pixels[offset..offset + 3].fill(255);
                            }
                        }
                    }
                }
            }
            assert_eq!(
                td_engine::sha256::hex_digest(&pixels[top * 3840..(top + 14) * 3840]),
                captured
            );
        }
        let mut bytes = b"P6\n1280 800\n255\n".to_vec();
        bytes.extend_from_slice(&pixels);
        assert!(menu_matches(&bytes).unwrap());
        let entry = b"P6\n1280 800\n255\n".len() + 456 * 3840;
        bytes[entry..entry + 14 * 3840].fill(0);
        assert!(!menu_matches(&bytes).unwrap());
    }

    /// Every chrome glyph the oracles draw is the compositor's own.
    #[test]
    fn chrome_glyphs_are_the_compositors() {
        let chrome = include_str!("../../../../../../td-compositor/src/ui.rs");
        for character in b" :ABCDEFGHIJKLMNOPQRSTUVWXYZ-" {
            let rows = chrome_glyph(*character).unwrap();
            let line = format!(
                "b'{}' => [{}],",
                char::from(*character),
                rows.map(|row| row.to_string()).join(", ")
            );
            assert!(chrome.contains(&line), "{line}");
        }
        assert!(chrome_glyph(b'a').is_err());
        assert!(chrome_glyph(b'0').is_err());
        assert!(chrome.contains("pub(crate) const GLYPH_ADVANCE: usize = 6;"));
        assert!(chrome.contains("pub(crate) const GLYPH_WIDTH: usize = 5;"));
    }

    /// The bar oracle's band, cell, colours and blend are the compositor's.
    #[test]
    fn the_bar_text_is_the_compositors() {
        let bar = include_str!("../../../../../../td-compositor/src/bar.rs");
        for line in [
            "pub const BAR_HEIGHT: usize = 24;",
            "const TEXT_TOP: usize = (BAR_HEIGHT - CELL_HEIGHT) / 2;",
            "pub(crate) const BACKGROUND: [u8; 4] = [0x18, 0x14, 0x20, 0];",
            "pub(crate) const INK: [u8; 4] = [0xd0, 0xc8, 0xe0, 0];",
            "[net, load, memory, uptime, clock].join(SEPARATOR)",
            // The band is the screen's top, and the line is drawn in it at
            // the text row, in ink on the background, after the cells.
            "let bar = (0, 0, width, BAR_HEIGHT);",
            "        (text_left, TEXT_TOP),\n        text,\n        (INK, BACKGROUND),\n",
        ] {
            assert!(bar.contains(line), "{line}");
        }
        let text = include_str!("../../../../../../td-compositor/src/text.rs");
        for line in [
            "pub(crate) const CELL_WIDTH: usize = 8;",
            "pub(crate) const CELL_HEIGHT: usize = 16;",
            "Face::fit(regular, None, CELL_WIDTH, CELL_HEIGHT)",
            "let slot = face.glyph(Style::Regular, character);",
            "let cell = crate::ui::intersect((left, y, CELL_WIDTH, CELL_HEIGHT), clip);",
            "target.blend(page, entry, pen, ink, ground);",
            "Some((Slot::Missing, _)) | None => target.bitmap(character, (left, y), ink),",
            "self.put(x, y, mix(ground, ink, alpha));",
            "(u16::from(*from) * (255 - alpha) + u16::from(*to) * alpha + 127) / 255",
        ] {
            assert!(text.contains(line), "{line}");
        }
        assert_eq!(BAR_TEXT_TOP, (BAR_HEIGHT - BAR_CELL_HEIGHT) / 2);
        assert_eq!((BAR_CELL_WIDTH, BAR_CELL_HEIGHT), (8, 16));
        // The image's face is the one the compositor reads.
        let system = include_str!("../../../../../../recipes/src/recipes/system-x86-64.rs");
        assert!(
            system.contains("\"{in:jetbrains-mono-nerd-font}/share/fonts/jetbrains-mono-nerd\"")
        );
        assert_eq!(crate::face_file::DIR, "/etc/fonts/jetbrains-mono-nerd");
        assert_eq!(
            td_recipe::catalog::outline_face::DIR,
            "share/fonts/jetbrains-mono-nerd"
        );
    }

    /// A face with only a square for `0` and an empty space: what the face
    /// has is its coverage, anything else Unifont's cell.
    fn square_face() -> Vec<u8> {
        use crate::test_fonts::{square, Builder, Glyph, Segment};
        let mut builder = Builder::new(vec![
            Glyph::Empty,
            Glyph::Simple(vec![square(50, 0, 400)]),
            Glyph::Empty,
        ]);
        let map = |scalar: char, glyph: u16| {
            let code = u32::from(scalar) as u16;
            Segment::Delta(code, code, glyph.wrapping_sub(code))
        };
        builder.format4 = vec![map(' ', 2), map('0', 1)];
        builder.advance = Some(500);
        builder.font()
    }

    /// Each character is drawn in its own cell, clipped to it: the face's
    /// coverage blended toward the ink, Unifont's cell in ink otherwise.
    #[test]
    fn the_bar_text_draws_the_face_and_unifont_in_their_cells() {
        let drawn = BarText::render(square_face(), " 0U").unwrap();
        assert_eq!(drawn.width, 24);
        let at = |x: usize, y: usize| drawn.pixels[y * 24 + x];
        // The space marks nothing; the band outside the cell row is clear.
        for y in 0..BAR_HEIGHT {
            for x in 0..8 {
                assert_eq!(at(x, y), BAR_BACKGROUND);
            }
            if !(BAR_TEXT_TOP..BAR_TEXT_TOP + BAR_CELL_HEIGHT).contains(&y) {
                for x in 0..24 {
                    assert_eq!(at(x, y), BAR_BACKGROUND, "({x}, {y})");
                }
            }
        }
        // Fitted to the cell, 1000 units a em at a 500-unit advance is 16
        // pixels a em: the pen at 0 and the baseline at (16 / 2 + 300 *
        // 0.016).round() = 13. The square, x 50..450 and y 0..400 units, is
        // then x 0.8..7.2 and 6.4 pixels up from row 13: ink in columns 1 to
        // 6 of rows 7 to 12, partly covered beside them, nothing else.
        for row in 0..BAR_CELL_HEIGHT {
            for column in 0..BAR_CELL_WIDTH {
                let pixel = at(8 + column, BAR_TEXT_TOP + row);
                let full_row = (7..=12).contains(&row);
                let full_column = (1..=6).contains(&column);
                if full_row && full_column {
                    assert_eq!(pixel, BAR_INK, "({column}, {row})");
                } else if (6..=12).contains(&row) {
                    assert_ne!(pixel, BAR_INK, "({column}, {row})");
                    assert_ne!(pixel, BAR_BACKGROUND, "({column}, {row})");
                    for ((channel, from), to) in pixel.iter().zip(BAR_BACKGROUND).zip(BAR_INK) {
                        assert!((from.min(to)..=from.max(to)).contains(channel));
                    }
                } else {
                    assert_eq!(pixel, BAR_BACKGROUND, "({column}, {row})");
                }
            }
        }
        // U is not in the face, so it is Unifont's cell, exactly.
        let unifont = crate::font::pinned().unwrap();
        let index = unifont.index('U');
        let mut lit = 0;
        for row in 0..BAR_CELL_HEIGHT {
            for column in 0..BAR_CELL_WIDTH {
                let want = if unifont.pixel(index, column, row) {
                    lit += 1;
                    BAR_INK
                } else {
                    BAR_BACKGROUND
                };
                assert_eq!(at(16 + column, BAR_TEXT_TOP + row), want);
            }
        }
        assert!(lit > 0);
        let last = (16..24)
            .rev()
            .find(|&x| (0..BAR_HEIGHT).any(|y| at(x, y) != BAR_BACKGROUND))
            .unwrap();
        assert_eq!(drawn.last, last);
        assert_eq!(mix(BAR_BACKGROUND, BAR_INK, 0), BAR_BACKGROUND);
        assert_eq!(mix(BAR_BACKGROUND, BAR_INK, 255), BAR_INK);
        assert_eq!(mix([0, 0, 0], [255, 255, 255], 128), [128, 128, 128]);
        // Rounded, not truncated: 128 / 255 of one step is one.
        assert_eq!(mix([0, 0, 0], [1, 1, 1], 128), [1, 1, 1]);
        assert_eq!(mix([0, 0, 0], [1, 1, 1], 127), [0, 0, 0]);
        // A face that is not a face is an error, not a blank expectation.
        assert!(BarText::render(vec![0; 64], " 0").is_err());
        assert!(BarText::render(square_face(), "   ").is_err());
    }

    /// The bar's line is matched at its end, exactly, and nowhere else.
    #[test]
    fn the_bar_line_is_matched_at_its_end() {
        let face = square_face();
        // Each character is clipped to its cell, so the whole line drawn at
        // once is each character drawn where the compositor draws it.
        let bar = |text: &str, left: usize| {
            let mut pixels = [24, 32, 40].repeat(1280 * 800);
            let line = BarText::render(face.clone(), text).unwrap();
            for y in 0..24 {
                for x in 0..1280usize {
                    let color = match x.checked_sub(left) {
                        Some(column) if column < line.width => line.pixels[y * line.width + column],
                        _ => [0x20, 0x14, 0x18],
                    };
                    pixels[(y * 1280 + x) * 3..(y * 1280 + x) * 3 + 3].copy_from_slice(&color);
                }
            }
            pixels
        };
        let zone = BarText::render(face.clone(), " UTC+09:00").unwrap();
        let line = "NET ETH0 DOWN  UP 3M  2026-10-02 10:00:00 UTC+09:00";
        for left in [8, 39, 100] {
            assert!(zone.ends(&bar(line, left)).unwrap());
        }
        let drawn = bar(line, 8);
        for other in [" UTC+08:00", " UTC-09:00", "UTC+09:0", " UTC+90:00"] {
            let other_text = BarText::render(face.clone(), other).unwrap();
            assert!(!other_text.ends(&drawn).unwrap(), "{other}");
        }
        assert!(!zone.ends(&bar("2026-10-02 10:00:00 UTC", 8)).unwrap());
        // Text after it, or a stray pixel beside it, is not this end.
        assert!(!zone.ends(&bar(&format!("{line}0"), 8)).unwrap());
        let mut stray = drawn.clone();
        let x = 8 + line.len() * 8 + 1;
        stray[(6 * 1280 + x) * 3..(6 * 1280 + x) * 3 + 3].copy_from_slice(&[0xe0, 0xc8, 0xd0]);
        assert!(!zone.ends(&stray).unwrap());
        // Nor is a mark of another colour after it, such as a pointer.
        let mut marked = drawn.clone();
        marked[(2 * 1280 + 1270) * 3..(2 * 1280 + 1270) * 3 + 3].copy_from_slice(&[255, 0, 0]);
        assert!(!zone.ends(&marked).unwrap());
        // Nor one pixel of the text itself off by a shade.
        let mut shaded = drawn.clone();
        let (x, y) = (BAR_TEXT_TOP..BAR_TEXT_TOP + BAR_CELL_HEIGHT)
            .flat_map(|y| (0..8).map(move |column| (8 + (line.len() - 1) * 8 + column, y)))
            .find(|&(x, y)| {
                shaded[(y * 1280 + x) * 3..(y * 1280 + x) * 3 + 3] != [0x20, 0x14, 0x18]
            })
            .unwrap();
        shaded[(y * 1280 + x) * 3] ^= 1;
        assert!(!zone.ends(&shaded).unwrap());
        assert!(!zone.ends(&[24, 32, 40].repeat(1280 * 800)).unwrap());
        assert!(zone.ends(&drawn[3..]).is_err());
    }

    /// A notice row drawn from the compositor's own chrome glyphs, hyphen
    /// and plus included, matches only where and as it is drawn.
    #[test]
    fn menu_notice_rows_match_the_chrome_font_with_its_hyphen() {
        for text in [
            "SYSTEM INSTALLED - RESTART TO BOOT IT",
            "PRESS CTRL+ALT+ESC TO UNLOCK",
        ] {
            notice_row_matches_the_chrome_font(text);
        }
    }

    fn notice_row_matches_the_chrome_font(text: &str) {
        let chrome = include_str!("../../../../../../td-compositor/src/ui.rs");
        let top = 312;
        let mut pixels = [24, 32, 40].repeat(1280 * 800);
        for (column, character) in text.chars().enumerate() {
            let prefix = format!("b'{character}' => [");
            let rows = chrome
                .lines()
                .find_map(|line| line.trim().strip_prefix(&prefix)?.strip_suffix("],"))
                .unwrap_or("0, 0, 0, 0, 0, 0, 0");
            for (y, bits) in rows
                .split(',')
                .map(|row| row.trim().parse::<u8>().unwrap())
                .enumerate()
            {
                for x in 0..5 {
                    if bits & (1 << (4 - x)) == 0 {
                        continue;
                    }
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let offset =
                                ((top + y * 2 + dy) * 1280 + 24 + column * 12 + x * 2 + dx) * 3;
                            pixels[offset..offset + 3].fill(255);
                        }
                    }
                }
            }
        }
        assert!(menu_row_matches(&pixels, top, text).unwrap());
        assert!(!menu_row_matches(&pixels, top + 36, text).unwrap());
        let blanked = text.replace(['-', '+'], " ");
        assert!(!menu_row_matches(&pixels, top, &blanked).unwrap());
        assert!(menu_row_matches(&pixels, top, "a").is_err());
    }

    #[test]
    fn physical_confirmation_requires_the_complete_expected_prompt() {
        let id = "ab".repeat(32);
        let image = screenshot(&id);
        assert!(prompt_matches(&image, &id).unwrap());
        assert!(!prompt_matches(&image, &"cd".repeat(32)).unwrap());
        for row in 0..7 {
            let mut corrupt = image.clone();
            let offset = b"P6\n1280 800\n255\n".len() + (244 + row * 40) * 1280 * 3;
            corrupt[offset..offset + 32 * 1280 * 3].fill(0);
            assert!(!prompt_matches(&corrupt, &id).unwrap(), "missing row {row}");
        }
        assert!(prompt_matches(&image[..image.len() - 1], &id).is_err());
    }

    #[test]
    #[ignore = "requires an actual QMP installation prompt capture"]
    fn captured_installation_prompt_matches() {
        let path = std::env::var("TD_UPDATE_PROMPT_CAPTURE").unwrap();
        let id = std::env::var("TD_UPDATE_PROMPT_ID").unwrap();
        assert!(prompt_matches(&read_capture(Path::new(&path)).unwrap(), &id).unwrap());
    }

    #[test]
    fn kernel_handoff_requires_one_complete_deployment_token() {
        let id = "ab".repeat(32);
        assert_eq!(
            kernel_id(&format!("console=ttyS0 td.deployment={id} quiet")),
            Some(id.as_str())
        );
        assert!(kernel_id(&format!("td.deployment={id} td.deployment={id}")).is_none());
        assert!(kernel_id(&format!("td.deployment={id}0")).is_none());
        assert!(kernel_id(&format!("prefix-td.deployment={id}")).is_none());
    }

    #[test]
    fn serial_results_survive_crlf_and_a_closed_consumer_unblocks_the_reader() {
        let sequence = std::sync::atomic::AtomicU64::new(16000);
        let dir = super::super::create_scratch_dir(&std::env::temp_dir(), &sequence).unwrap();
        let _scratch = super::super::Scratch { dir: dir.clone() };
        let (send, receive) = mpsc::sync_channel(1);
        let file = private_file(&dir.join("serial.log")).unwrap();
        let reader = thread::spawn(move || {
            serial_lines(&b"redraw\rRESULT=0\r\nsecond\nthird\n"[..], file, send)
        });
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            "RESULT=0"
        );
        drop(receive);
        assert!(reader.join().unwrap().is_err());
    }

    #[test]
    fn duplicate_or_unknown_options_are_rejected_before_opening_inputs() {
        for args in [
            vec!["--kernel"],
            vec!["--unknown", "value"],
            vec!["--kernel", "missing", "--kernel", "missing"],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(options(&args).err().unwrap(), USAGE);
        }
    }

    #[test]
    fn accelerator_defaults_to_tcg_and_refuses_implicit_fallbacks() {
        let input = std::env::current_exe().unwrap().display().to_string();
        let mut args = vec![
            "--kernel".into(),
            input.clone(),
            "--selector".into(),
            input.clone(),
            "--disk".into(),
            input,
            "--format".into(),
            "qcow2".into(),
            "--work".into(),
            "unused".into(),
        ];
        assert_eq!(options(&args).unwrap().accel, "tcg");
        args.extend(["--accel".into(), "kvm".into()]);
        assert_eq!(options(&args).unwrap().accel, "kvm");
        *args.last_mut().unwrap() = "tcg".into();
        assert_eq!(options(&args).unwrap().accel, "tcg");
        for invalid in ["kvm:tcg", "tcg,thread=multi", "auto", ""] {
            *args.last_mut().unwrap() = invalid.into();
            assert_eq!(options(&args).err().unwrap(), USAGE);
        }
    }

    #[test]
    fn source_object_ids_support_sha1_and_sha256() {
        for length in [40, 64] {
            assert!(git_id(&"a".repeat(length)));
        }
        for bad in [
            "a".repeat(39),
            "a".repeat(65),
            "g".repeat(40),
            "A".repeat(64),
        ] {
            assert!(!git_id(&bad));
        }
    }

    #[test]
    fn admission_receipt_matches_the_actual_authority_notice() {
        let notice = "Update {deployment} is ready. Press Ctrl+Alt+Escape, then I to review it.";
        assert!(include_str!("../../../../../../td-authd/src/deployment.rs").contains(notice));
        let id = "ab".repeat(32);
        assert_eq!(
            ready_id(&notice.replace("{deployment}", &id)),
            Some(id.as_str())
        );
    }

    #[test]
    fn admission_receipt_requires_one_complete_canonical_id() {
        let id = "ab".repeat(32);
        assert_eq!(
            ready_id(&format!("Update {id}{READY_NOTICE}")),
            Some(id.as_str())
        );
        for bad in [
            format!("Update {} is ready.", id.to_uppercase()),
            format!("prefix Update {id} is ready."),
            format!("Update {id} is ready. suffix"),
            "Update ab is ready.".into(),
            format!("Update {id} is ready."),
        ] {
            assert!(ready_id(&bad).is_none());
        }
    }
}
