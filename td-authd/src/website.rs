//! The paired compositor controls one virtual authenticator per generation.
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Eq, PartialEq)]
pub(crate) struct Frame(Vec<u8>);
impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("website authenticator request")
    }
}
impl Drop for Frame {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}
impl Frame {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        match bytes {
            [1] | [3] => (),
            [2, tail @ ..] => {
                let (_, data) = tail
                    .split_at_checked(32)
                    .ok_or("missing worker generation")?;
                let (&verb, rest) = data.split_first().ok_or("missing website operation")?;
                let (_, args) = rest.split_at_checked(8).ok_or("missing website ticket")?;
                match (verb, args) {
                    (3 | 9 | 15, []) | (5, [0..=1]) | (7 | 13, [_]) | (8, []) => (),
                    (4, a) if a.len() == 9 => (),
                    (8 | 14, a) if a.len() == 32 => (),
                    (6 | 12, pin)
                        if (4..=63).contains(&pin.len())
                            && pin.iter().all(|b| (0x20..=0x7e).contains(b)) =>
                    {
                        ()
                    }
                    _ => return Err("invalid website operation arguments".into()),
                }
            }
            _ => return Err("invalid website control frame".into()),
        }
        Ok(Self(
            crate::allocation::copy(bytes).map_err(|_| String::new())?,
        ))
    }
}

struct Worker {
    child: Child,
    control: UnixDatagram,
    epoch: [u8; 32],
    starting: Option<Instant>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Worker {
    fn start(group: u32) -> Result<Self, String> {
        let epoch = crate::login::draw()?;
        let (control, peer) = UnixDatagram::pair().map_err(|_| "website control socketpair")?;
        let timeout = Some(Duration::from_millis(400));
        control
            .set_read_timeout(timeout)
            .and_then(|()| control.set_write_timeout(timeout))
            .map_err(|_| "website control timeout")?;
        let child = Command::new("/bin/td-pass")
            .args([
                "website-authenticator",
                &crate::text(format_args!("{group}"))?,
            ])
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(OwnedFd::from(peer)))
            .stdout(Stdio::null())
            .spawn()
            .map_err(|_| "start website authenticator")?;
        let worker = Self {
            child,
            control,
            epoch,
            starting: Some(Instant::now()),
        };
        worker
            .control
            .set_nonblocking(true)
            .map_err(|_| "website startup socket")?;
        if worker
            .control
            .send(&[1])
            .map_err(|_| "website startup probe")?
            != 1
        {
            return Err("partial website startup probe".into());
        }
        Ok(worker)
    }
    fn ready(&mut self) -> Result<bool, String> {
        let Some(start) = self.starting else {
            return Ok(true);
        };
        if start.elapsed() >= Duration::from_secs(6)
            || self
                .child
                .try_wait()
                .map_err(|_| "website child status")?
                .is_some()
        {
            return Err("website authenticator unavailable".into());
        }
        let mut reply = [0; 2049];
        match self.control.recv(&mut reply) {
            Ok(n) if (1..=2048).contains(&n) => {
                self.control
                    .set_nonblocking(false)
                    .map_err(|_| "website socket mode")?;
                self.starting = None;
                Ok(true)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            _ => Err("website startup failed".into()),
        }
    }
    fn exchange(&mut self, q: &[u8]) -> Result<Vec<u8>, String> {
        let mut buf = [0; 2049];
        if self.control.send(q).map_err(|_| "send website request")? != q.len() {
            return Err("partial website request".into());
        }
        let n = self
            .control
            .recv(&mut buf)
            .map_err(|_| "receive website answer")?;
        let body = buf
            .get(..n)
            .filter(|b| !b.is_empty() && b.len() <= 2048)
            .ok_or("invalid website answer")?;
        let mut response = crate::allocation::with_capacity(n + 33).map_err(|_| String::new())?;
        crate::allocation::push(&mut response, 0xad)
            .and_then(|()| crate::allocation::extend_from_slice(&mut response, &self.epoch))
            .and_then(|()| crate::allocation::extend_from_slice(&mut response, body))
            .map_err(|_| String::new())?;
        Ok(response)
    }
}

#[derive(Default)]
pub(crate) struct Authority {
    worker: Option<Worker>,
    attempted: Option<Instant>,
    /// The worker's last status named a ticket the physical seat has taken
    /// up: a website or management ceremony it is driving.
    active: bool,
    /// A test's stand-in for a ceremony in flight.
    #[cfg(test)]
    pub(crate) held: bool,
    #[cfg(test)]
    pub(crate) revocations: u32,
}
impl Authority {
    /// Whether a ceremony was in flight at the last poll. A notebook
    /// operation waits for it (td-authd/DESIGN.md, "Notebook token
    /// descriptions").
    pub fn busy(&self) -> bool {
        #[cfg(test)]
        if self.held {
            return true;
        }
        self.worker.is_some() && self.active
    }
    pub fn revoke(&mut self) {
        self.active = false;
        #[cfg(test)]
        {
            self.held = false;
            self.revocations += 1;
        }
        if self
            .worker
            .as_mut()
            .is_some_and(|w| w.starting.is_some() || w.exchange(&[2]).is_err())
        {
            self.worker = None;
        }
    }
    pub fn answer(&mut self, frame: Frame, allowed: bool) -> Result<Vec<u8>, String> {
        let idle = || crate::allocation::copy(&[0xad]).map_err(|_| String::new());
        let polling = frame.0 == [1];
        if !allowed {
            if polling {
                self.revoke();
            }
            return idle();
        }
        if polling
            && self.worker.is_none()
            && self
                .attempted
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
        {
            self.attempted = Some(Instant::now());
            if let Ok(group) = crate::application::admitted_uid("firefox") {
                self.worker = Worker::start(group).ok();
            }
        }
        let Some(worker) = &mut self.worker else {
            return idle();
        };
        match worker.ready() {
            Ok(true) => (),
            Ok(false) => return idle(),
            Err(_) => {
                self.worker = None;
                return idle();
            }
        }
        let q = if frame.0 == [3] {
            &[11][..]
        } else if polling {
            &[1][..]
        } else {
            let (_, tail) = frame.0.split_first().ok_or("missing website request")?;
            let (epoch, q) = tail
                .split_at_checked(32)
                .ok_or("missing website generation")?;
            if epoch != worker.epoch {
                return idle();
            }
            q
        };
        match worker.exchange(q) {
            Ok(answer) => {
                if polling {
                    self.active = taken_up(&answer);
                }
                Ok(answer)
            }
            Err(_) => {
                self.worker = None;
                self.active = false;
                idle()
            }
        }
    }
}

/// Whether a status names a ticket the seat has taken up. The status is
/// `ad`, the generation, the result byte and, while a ticket is open, the
/// ticket and its kind; kinds 0-3 are an offer still waiting for the seat,
/// which a page may hold open, so it does not hold the notebook back.
fn taken_up(status: &[u8]) -> bool {
    status.get(42).is_some_and(|kind| *kind > 3)
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_ticket_the_seat_took_up_holds_the_notebook() {
        let status = |tail: &[u8]| {
            let mut bytes = vec![0xad];
            bytes.extend_from_slice(&[7; 32]);
            bytes.push(0);
            bytes.extend_from_slice(tail);
            bytes
        };
        assert!(!super::taken_up(&status(&[])));
        for offer in 0..=3 {
            assert!(!super::taken_up(&status(
                &[[9; 8].as_slice(), &[offer, 1, 2]].concat()
            )));
        }
        for kind in [6, 7, 8, 10, 11, 12, 13, 14, 15, 16] {
            assert!(super::taken_up(&status(
                &[[9; 8].as_slice(), &[kind]].concat()
            )));
        }
    }
}
