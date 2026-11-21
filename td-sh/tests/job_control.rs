#![forbid(unsafe_code)]

use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::sync::mpsc::{sync_channel, Receiver};
use std::time::{Duration, Instant};
use td_ui::{pty, vt_keys};

struct Session(Child);

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Transcript {
    output: Receiver<pty::Output>,
    pending: Vec<u8>,
}

impl Transcript {
    fn until(&mut self, marker: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(at) = self
                .pending
                .windows(marker.len())
                .position(|bytes| bytes == marker)
            {
                self.pending.drain(..at + marker.len());
                return;
            }
            match self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(pty::Output::Bytes(bytes)) => self.pending.extend(bytes),
                other => panic!(
                    "waiting for {:?}: {other:?}, read {:?}",
                    String::from_utf8_lossy(marker),
                    String::from_utf8_lossy(&self.pending)
                ),
            }
        }
    }
}

#[test]
fn terminal_suspend_and_resume() -> Result<(), Box<dyn std::error::Error>> {
    let directory = td_fs::TempDir::new("td-sh-job-control")?;
    let shell = Path::new(env!("CARGO_BIN_EXE_td-sh"));
    std::os::unix::fs::symlink(shell, directory.path().join("child"))?;
    let terminal = pty::Pty::open()?;
    let _session = Session(pty::spawn(
        &pty::ChildCommand {
            program: shell.into(),
            arguments: vec![],
            leads_session: true,
        },
        &[("PS1", "PROMPT "), ("HISTFILE", "")],
        directory.path(),
        terminal.peer()?,
    )?);
    let (send, output) = sync_channel(32);
    let _reader = pty::spawn_reader(terminal.master().try_clone()?, send, |o| o, || {})?;
    let mut transcript = Transcript {
        output,
        pending: vec![],
    };
    let mut input = terminal.master();
    transcript.until(b"PROMPT ");
    input.write_all(b"./child -c 'echo REA''DY; read line; echo got=$line; exit 7'\n")?;
    transcript.until(b"READY\r\n");
    input.write_all(
        vt_keys::sequence("C-z", Default::default())
            .unwrap()
            .as_slice(),
    )?;
    transcript.until(b"PROMPT ");
    input.write_all(b"jobs\n")?;
    transcript.until(b"Stopped");
    input.write_all(b"bg %1 && echo back''grounded\n")?;
    transcript.until(b"backgrounded\r\n");
    input.write_all(b"fg %1; echo status=$?\nhello\n")?;
    transcript.until(b"got=hello\r\n");
    transcript.until(b"status=7\r\n");
    // A resumed background job can keep working while the prompt accepts input.
    std::fs::write(directory.path().join("alive"), b"")?;
    input.write_all(b"./child -c 'echo WOR''KING; while [ -f alive ] && [ ! -f proceed ]; do :; done; exit 12'\n")?;
    transcript.until(b"WORKING\r\n");
    input.write_all(
        vt_keys::sequence("C-z", Default::default())
            .unwrap()
            .as_slice(),
    )?;
    transcript.until(b"PROMPT ");
    input.write_all(b"bg && : > proceed; wait $!; echo waited=$?\n")?;
    transcript.until(b"waited=12\r\n");
    // Suspending an explicitly selected older job makes it current again.
    let suspend = vt_keys::sequence("C-z", Default::default()).unwrap();
    for (command, ready) in [
        (
            "./child -c 'echo FI''RST; read a; echo reading=$a; read b; echo first=$b'\n",
            b"FIRST\r\n".as_slice(),
        ),
        (
            "./child -c 'echo SEC''OND; read b; echo second=$b'\n",
            b"SECOND\r\n".as_slice(),
        ),
    ] {
        input.write_all(command.as_bytes())?;
        transcript.until(ready);
        input.write_all(suspend.as_slice())?;
        transcript.until(b"PROMPT ");
    }
    input.write_all(b"fg %1\nagain\n")?;
    transcript.until(b"reading=again\r\n");
    input.write_all(suspend.as_slice())?;
    transcript.until(b"PROMPT ");
    input.write_all(b"fg\none\n")?;
    transcript.until(b"first=one\r\n");
    input.write_all(b"fg\ntwo\n")?;
    transcript.until(b"second=two\r\n");
    // Back-to-back bg commands each resume a different stopped job.
    for end in ["finish-one", "finish-two"] {
        let after = if end == "finish-one" {
            "read value; echo value=$value"
        } else {
            ":"
        };
        writeln!(input, "./child -c 'echo PARK''ED; while [ -f alive ] && [ ! -f {end} ]; do :; done; echo done-{end}; {after}'")?;
        transcript.until(b"PARKED\r\n");
        input.write_all(suspend.as_slice())?;
        transcript.until(b"PROMPT ");
    }
    input.write_all(
        b"bg; first=$!; bg %+; second=$!; [ $first != $second ] && echo bo''th; : > finish-two\n",
    )?;
    transcript.until(b"both\r\n");
    transcript.until(b"done-finish-two\r\n");
    // Wait for the shell to collect the exit after the child's final write.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        input.write_all(b"jobs > states; echo lis''ted\n")?;
        transcript.until(b"listed\r\n");
        if std::fs::read_to_string(directory.path().join("states"))?.contains("Done") {
            break;
        }
        assert!(Instant::now() < deadline, "the second job did not finish");
    }
    // Keep the newer exit status listed while the older group is still alive.
    input.write_all(b"fg; echo foreground=$?\n")?;
    std::fs::write(directory.path().join("finish-one"), b"")?;
    transcript.until(b"done-finish-one\r\n");
    input.write_all(b"resumed\n")?;
    transcript.until(b"value=resumed\r\n");
    transcript.until(b"foreground=0\r\n");
    Ok(())
}
