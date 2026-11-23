//! Own the WAL receiver until it flushes or a recovery budget is exhausted.
use super::{CONTROL, GIB};
use crate::common::{child, fmt, free_bytes, require, Host};
use crate::R;
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Receiving(Child);

impl Receiving {
    fn finish(&mut self, host: &Host) -> R<()> {
        if let Some(status) = self.0.try_wait()? {
            return require(status.success(), "WAL receiver failed");
        }
        // This is our unreaped child; its PID cannot be recycled underneath
        // the signal. SIGINT makes pg_receivewal flush its current segment.
        let signal = host
            .cmd("kill")
            .arg("-INT")
            .arg(fmt(format_args!("{}", self.0.id()))?)
            .status()?;
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.0.try_wait()? {
                return require(status.success(), "WAL receiver failed");
            }
            require(signal.success(), "cannot signal WAL receiver")?;
            require(Instant::now() < end, "WAL receiver shutdown timed out")?;
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Receiving {
    fn drop(&mut self) {
        // Also covers failure to start the monitor thread after spawning the
        // child and a receiver that ignored the graceful shutdown deadline.
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spool_size(wal: &Path) -> R<u64> {
    let mut size = 0u64;
    for entry in fs::read_dir(wal)? {
        let entry = entry?;
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            // pg_receivewal renames a completed segment while we enumerate.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if metadata.is_file() {
            size = size
                .checked_add(metadata.len())
                .ok_or("WAL spool size overflow")?;
        }
    }
    Ok(size)
}

pub(super) struct Receiver {
    stop: mpsc::Sender<()>,
    health: mpsc::Receiver<()>,
    monitor: Option<JoinHandle<R<()>>>,
}

impl Receiver {
    pub(super) fn start(host: &Host, work: &Path) -> R<Self> {
        let wal = child(work, "wal")?;
        let output = File::create(child(work, "receiver.log")?)?;
        let mut child_process = host.cmd(host.tool("pg_receivewal")?);
        child_process
            .args(["-wn", "--verbose", "--synchronous", "-D"])
            .arg(&wal)
            .args(["-d", "service=cutover_repl application_name=td_cutover_wal"])
            .env("PGSERVICEFILE", child(Path::new(CONTROL), "service.conf")?)
            .stdin(Stdio::null())
            .stderr(output.try_clone()?)
            .stdout(output);
        let mut process = Receiving(child_process.spawn()?);
        let start = Instant::now();
        let (stop, requested) = mpsc::channel();
        let (healthy, health) = mpsc::channel::<()>();
        let host = Host::default();
        let monitor = thread::Builder::new().spawn(move || -> R<()> {
            let check = (|| -> R<()> {
                while matches!(
                    requested.recv_timeout(Duration::from_millis(500)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    require(
                        start.elapsed() < Duration::from_secs(1800),
                        "physical backup preparation timeout",
                    )?;
                    require(
                        free_bytes(&wal)? > 8 * GIB,
                        "physical recovery disk reserve",
                    )?;
                    require(spool_size(&wal)? <= GIB, "WAL spool budget")?;
                    require(process.0.try_wait()?.is_none(), "WAL receiver disconnected")?;
                }
                Ok(())
            })();
            // Closing the channel makes a failed guard visible throughout
            // receiver shutdown, including its ten-second flush deadline.
            drop(healthy);
            let stopped = process.finish(&host);
            check.and(stopped)
        })?;
        Ok(Self {
            stop,
            health,
            monitor: Some(monitor),
        })
    }

    pub(super) fn healthy(&self) -> R<()> {
        require(
            matches!(self.health.try_recv(), Err(mpsc::TryRecvError::Empty))
                && self.monitor.as_ref().is_some_and(|m| !m.is_finished()),
            "WAL receiver or storage guard failed",
        )
    }

    pub(super) fn stop(mut self) -> R<()> {
        let _ = self.stop.send(());
        let monitor = self.monitor.take().ok_or("missing WAL receiver")?;
        JoinHandle::join(monitor).map_err(|_| "WAL receiver guard terminated")?
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.monitor.take() {
            let _ = JoinHandle::join(thread);
        }
    }
}
