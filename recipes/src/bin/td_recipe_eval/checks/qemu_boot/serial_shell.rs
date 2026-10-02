//! Types into the installed system's serial login shell: once a boot has
//! reached its marker and the greeter has logged the primary account in on
//! ttyS0, each step sends one command line and waits for the one report
//! line it prints, at the start of a line of its own. Neither the command's
//! echo nor the line editor's redraws, which begin with a carriage return,
//! start a line with its report: the command spells the report's newline
//! as `\n`, so the report begins a line only when printf runs.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long the greeter and then every step may take after the marker.
pub(super) const SHELL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long QEMU may take to create the serial socket it waits on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One command line and the report it must print: `report`, a space and a
/// payload `accepts` takes.
pub(super) struct ShellStep {
    pub(super) command: String,
    pub(super) report: String,
    pub(super) accepts: Box<dyn Fn(&str) -> bool>,
}

/// The lines `console` has finished: a read can end inside a line whose
/// rest is still on its way.
fn complete_lines(console: &str) -> impl Iterator<Item = &str> {
    let end = console.rfind('\n').map_or(0, |at| at + 1);
    console
        .get(..end)
        .unwrap_or_default()
        .lines()
        .map(str::trim_end)
}

/// Whether `console` has answered a step: its report lines' payloads, of
/// which there must be at most one, and that one accepted.
fn answered(step: &ShellStep, console: &str) -> Result<bool, String> {
    let prefix = format!("{} ", step.report);
    let reports: Vec<_> = complete_lines(console)
        .filter(|line| *line == step.report || line.starts_with(&prefix))
        .collect();
    match reports.as_slice() {
        [] => Ok(false),
        [line] => match line.strip_prefix(&prefix) {
            Some(payload) if (step.accepts)(payload) => Ok(true),
            _ => Err(format!("the serial shell reported {line:?}")),
        },
        _ => Err(format!("the serial shell reported {reports:?}, not once")),
    }
}

/// The oracle's end of ttyS0: QEMU waits for it before starting the guest,
/// so no console byte precedes it, and a thread drains what QEMU sends
/// (its log file is the console the boot loop reads).
pub(super) struct SerialPort {
    input: UnixStream,
    drain: Option<JoinHandle<()>>,
}

impl SerialPort {
    /// `running` says whether QEMU is still there to listen.
    pub(super) fn connect(path: &Path, running: &mut dyn FnMut() -> bool) -> Result<Self, String> {
        let until = Instant::now()
            .checked_add(CONNECT_TIMEOUT)
            .ok_or("serial deadline overflow")?;
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(stream) => break stream,
                Err(error) if !running() => {
                    return Err(format!(
                        "QEMU exited before its serial socket {} accepted: {error}",
                        path.display()
                    ))
                }
                Err(error) if Instant::now() >= until => {
                    return Err(format!("connect serial socket {}: {error}", path.display()))
                }
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        };
        Self::over(stream)
    }

    fn over(stream: UnixStream) -> Result<Self, String> {
        let mut output = stream
            .try_clone()
            .map_err(|error| format!("clone serial socket: {error}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| format!("bound serial writes: {error}"))?;
        let drain = thread::Builder::new()
            .name("td-serial-drain".into())
            .spawn(move || {
                let mut chunk = [0u8; 4096];
                while matches!(output.read(&mut chunk), Ok(read) if read > 0) {}
            })
            .map_err(|error| format!("start serial drain: {error}"))?;
        Ok(Self {
            input: stream,
            drain: Some(drain),
        })
    }

    fn send(&mut self, line: &str) -> Result<(), String> {
        self.input
            .write_all(line.as_bytes())
            .and_then(|()| self.input.write_all(b"\n"))
            .and_then(|()| self.input.flush())
            .map_err(|error| format!("type into the serial shell: {error}"))
    }

    /// Ends the drain: shutting the socket down ends its clone's read too.
    pub(super) fn finish(mut self) {
        let _ = self.input.shutdown(Shutdown::Both);
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
    }
}

impl Drop for SerialPort {
    fn drop(&mut self) {
        let _ = self.input.shutdown(Shutdown::Both);
    }
}

/// Runs the steps in order once the boot is ready for them.
pub(super) struct SerialShell<'a> {
    steps: &'a [ShellStep],
    next: usize,
    sent: bool,
    pub(super) until: Option<Instant>,
}

impl<'a> SerialShell<'a> {
    pub(super) fn new(steps: &'a [ShellStep]) -> Self {
        Self {
            steps,
            next: 0,
            sent: false,
            until: None,
        }
    }

    pub(super) fn done(&self) -> bool {
        self.next >= self.steps.len()
    }

    /// The step still awaited, for a failed boot's report.
    pub(super) fn awaiting(&self) -> Option<&str> {
        self.steps.get(self.next).map(|step| step.report.as_str())
    }

    /// The deadline starts at the marker; typing waits for the greeter.
    pub(super) fn poll(
        &mut self,
        port: &mut SerialPort,
        target: bool,
        greeter: bool,
        console: &[u8],
    ) -> Result<(), String> {
        if self.done() || !target {
            return Ok(());
        }
        let now = Instant::now();
        let until = match self.until {
            Some(until) => until,
            None => {
                let until = now
                    .checked_add(SHELL_TIMEOUT)
                    .ok_or("shell deadline overflow")?;
                self.until = Some(until);
                until
            }
        };
        if !greeter {
            if now >= until {
                return Err(format!(
                    "the greeter did not log the account in on ttyS0 within {}s of the marker",
                    SHELL_TIMEOUT.as_secs()
                ));
            }
            return Ok(());
        }
        let Some(step) = self.steps.get(self.next) else {
            return Ok(());
        };
        if !self.sent {
            port.send(&step.command)?;
            self.sent = true;
        }
        if answered(step, &String::from_utf8_lossy(console))? {
            self.next += 1;
            self.sent = false;
        } else if now >= until {
            return Err(format!(
                "the serial shell did not report {} within {}s of the marker",
                step.report,
                SHELL_TIMEOUT.as_secs()
            ));
        }
        Ok(())
    }

    /// Every step's report judged again on the console the boot ended
    /// with, so a report printed after its step was accepted still counts.
    pub(super) fn verify(&self, console: &str) -> Result<(), String> {
        for step in self.steps {
            if !answered(step, console)? {
                return Err(format!("the serial shell's {} report is gone", step.report));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(command: &str, report: &str, payload: &'static str) -> ShellStep {
        ShellStep {
            command: command.into(),
            report: report.into(),
            accepts: Box::new(move |seen| seen == payload),
        }
    }

    /// A step is answered by its one finished report line, at a line's
    /// start, however the console's reads split it.
    #[test]
    fn a_step_is_answered_by_its_one_finished_report() {
        let home = step("", "TD-SESSION-HOME-1", "dana /var/home/dana Asia/Tokyo");
        let echo = "\rdana@td-wizard:~$ printf '\\nTD-SESSION-HOME-1 %s\\n' x\n";
        assert_eq!(answered(&home, echo), Ok(false));
        let report = "TD-SESSION-HOME-1 dana /var/home/dana Asia/Tokyo\r\n";
        let whole = format!("{echo}{report}");
        for end in echo.len()..whole.len() {
            assert_eq!(answered(&home, &whole[..end]), Ok(false), "{end}");
        }
        assert_eq!(answered(&home, &whole), Ok(true));
        for wrong in [
            "TD-SESSION-HOME-1 dana /home/dana Asia/Tokyo\n",
            "TD-SESSION-HOME-1\n",
            "TD-SESSION-HOME-1 tester /var/home/tester Asia/Tokyo\n",
        ] {
            assert!(
                answered(&home, &format!("{echo}{wrong}")).is_err(),
                "{wrong}"
            );
        }
        assert!(answered(&home, &format!("{report}{report}")).is_err());
        // Another step's report is not this one's, however it begins.
        for other in [
            "TD-SESSION-HOME-2 nonce 256 .\n",
            "TD-SESSION-HOME-10 dana /var/home/dana Asia/Tokyo\n",
            "TD-SESSION-HOME-1dana /var/home/dana Asia/Tokyo\n",
        ] {
            assert_eq!(answered(&home, other), Ok(false), "{other}");
        }
    }

    /// Each command is typed once, after the marker and the greeter, the
    /// next only once the last is answered; the deadline runs from the
    /// marker; and the finished console is judged again.
    #[test]
    fn steps_are_typed_in_order_and_judged_again_at_the_end() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let typed = |theirs: &mut UnixStream| {
            let mut bytes = [0u8; 64];
            match theirs.read(&mut bytes) {
                Ok(read) => String::from_utf8_lossy(&bytes[..read]).into_owned(),
                Err(_) => String::new(),
            }
        };
        let mut port = SerialPort::over(ours).unwrap();
        let steps = [step("one", "A-1", "x"), step("two", "A-2", "y")];
        let mut shell = SerialShell::new(&steps);
        shell.poll(&mut port, false, true, b"").unwrap();
        assert!(shell.until.is_none());
        shell.poll(&mut port, true, false, b"").unwrap();
        assert!(shell.until.is_some());
        assert_eq!(typed(&mut theirs), "");
        shell.poll(&mut port, true, true, b"").unwrap();
        shell.poll(&mut port, true, true, b"A-1 x").unwrap();
        assert_eq!(typed(&mut theirs), "one\n");
        assert_eq!(shell.awaiting(), Some("A-1"));
        shell.poll(&mut port, true, true, b"A-1 x\n").unwrap();
        assert_eq!(shell.awaiting(), Some("A-2"));
        shell.poll(&mut port, true, true, b"A-1 x\nA-2 y").unwrap();
        assert_eq!(typed(&mut theirs), "two\n");
        assert!(!shell.done());
        shell
            .poll(&mut port, true, true, b"A-1 x\nA-2 y\n")
            .unwrap();
        assert!(shell.done());
        assert_eq!(typed(&mut theirs), "");
        assert!(shell.verify("A-1 x\nA-2 y\n").is_ok());
        assert!(shell.verify("A-1 x\nA-2 y\nA-1 x\n").is_err());
        assert!(shell.verify("A-1 x\nA-2 y\nA-2 z\n").is_err());
        assert!(shell.verify("A-2 y\n").is_err());
        // The marker starts the deadline, and a greeter that never comes
        // is named.
        let mut waiting = SerialShell::new(&steps);
        waiting.poll(&mut port, true, false, b"").unwrap();
        waiting.until = Some(Instant::now());
        let error = waiting.poll(&mut port, true, false, b"").unwrap_err();
        assert!(error.contains("greeter"), "{error}");
        let mut silent = SerialShell::new(&steps);
        silent.poll(&mut port, true, true, b"").unwrap();
        silent.until = Some(Instant::now());
        let error = silent.poll(&mut port, true, true, b"").unwrap_err();
        assert!(error.contains("did not report A-1"), "{error}");
        port.finish();
    }
}
