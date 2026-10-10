//! Bounded receiving transport turns; storage and handshake work run elsewhere.
use crate::{
    admission::timers::{self, TimeoutPlan, SESSION_SECONDS},
    config::routing::Routing,
    ports::{
        Clock, Commit, CommitFailure, Deadline, Error, FlushProgress, IoProgress, Tick, Transport,
    },
    smtp_session::{Pending, Session, Settings},
    transport::SOCKET_CHUNK,
};

const DRAIN_BYTES: usize = 4 * SOCKET_CHUNK;
const FINISH_SECONDS: u64 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progress {
    /// The transport would block; resume when scheduled again.
    Pending,
    /// One bounded turn completed; continue in the next fair scheduling round.
    Advanced,
    /// Inspect Session::pending and dispatch its storage or STARTTLS operation.
    Work,
    Closed,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Work {
    Begin,
    Data,
    Commit,
    Tls,
}

enum Closing {
    Open,
    Reply { deadline: Deadline },
    Shutdown { deadline: Deadline },
    Drain { deadline: Deadline, bytes: usize },
    Closed,
}

/// Keep this owner with the same session and transport for their entire life.
/// A pending reply may only change through this owner's flush acknowledgement.
/// Worker results may update the session only while advance returns Work.
/// Closing never disposes a storage job: its caller retires that job on a worker.
pub struct Network<'a> {
    session: Session<'a>,
    input: [u8; SOCKET_CHUNK],
    start: usize,
    end: usize,
    written: usize,
    work: Option<Work>,
    reply_grace: Option<Deadline>,
    session_deadline: Deadline,
    idle_deadline: Deadline,
    data_deadline: Option<Deadline>,
    idle_seconds: u64,
    data_seconds: u64,
    last: Tick,
    closing: Closing,
}

impl<'a> Network<'a> {
    pub fn new(
        routes: &'a Routing<'a>,
        settings: Settings<'a>,
        plan: &TimeoutPlan,
        clock: &(impl Clock + ?Sized),
    ) -> Result<Self, Error> {
        let now = clock.sample()?.monotonic;
        Ok(Self {
            session: Session::new(routes, settings)?,
            input: [0; SOCKET_CHUNK],
            start: 0,
            end: 0,
            written: 0,
            work: None,
            reply_grace: None,
            session_deadline: after(now, SESSION_SECONDS)?,
            idle_deadline: after(now, plan.limits().smtp_idle_seconds)?,
            data_deadline: None,
            idle_seconds: plan.limits().smtp_idle_seconds,
            data_seconds: plan.limits().smtp_data_seconds,
            last: now,
            closing: Closing::Open,
        })
    }

    pub fn session(&self) -> &Session<'a> {
        &self.session
    }

    pub fn data_ready(
        &mut self,
        result: Result<(), Error>,
        clock: &(impl Clock + ?Sized),
    ) -> Result<(), Error> {
        self.complete_work(Work::Begin, clock)?;
        self.session.data_ready(result)?;
        if self.session.envelope().is_none() {
            self.data_deadline = None;
        }
        Ok(())
    }

    pub fn data_written(
        &mut self,
        result: Result<(), Error>,
        clock: &(impl Clock + ?Sized),
    ) -> Result<(), Error> {
        let now = self.complete_work(Work::Data, clock)?;
        self.session.data_written(result)?;
        if result.is_err() {
            self.closing = Closing::Reply {
                deadline: after(now, FINISH_SECONDS)?,
            };
        }
        Ok(())
    }

    /// A known storage result gets a bounded reply even if its worker completed
    /// late. This grace never extends the lease used to admit or commit work.
    pub fn committed(
        &mut self,
        result: Result<Commit, CommitFailure>,
        clock: &(impl Clock + ?Sized),
    ) -> Result<(), Error> {
        let now = self.complete_work(Work::Commit, clock)?;
        self.session.committed(result)?;
        self.data_deadline = None;
        if matches!(self.session.pending(), Pending::Reply { .. }) {
            self.reply_grace = Some(after(now, FINISH_SECONDS)?);
        }
        Ok(())
    }

    pub fn message_too_large(&mut self, clock: &(impl Clock + ?Sized)) -> Result<(), Error> {
        let work = match self.work {
            Some(work @ (Work::Data | Work::Commit)) => work,
            _ => return Err(Error::Conflict),
        };
        let now = self.complete_work(work, clock)?;
        self.session.message_too_large()?;
        self.closing = Closing::Reply {
            deadline: after(now, FINISH_SECONDS)?,
        };
        Ok(())
    }

    /// The trusted TLS worker calls this only after an actual successful
    /// handshake on this connection. A failed upgrade must abort the owner.
    pub fn tls_established(&mut self, clock: &(impl Clock + ?Sized)) -> Result<(), Error> {
        self.complete_work(Work::Tls, clock)?;
        self.session.tls_established()
    }

    fn complete_work(
        &mut self,
        expected: Work,
        clock: &(impl Clock + ?Sized),
    ) -> Result<Tick, Error> {
        if !matches!(self.closing, Closing::Open) || self.work != Some(expected) {
            return Err(Error::Conflict);
        }
        let now = self.now(clock)?;
        self.idle_deadline = after(now, self.idle_seconds)?;
        self.work = None;
        Ok(now)
    }

    /// Request closure at an empty command boundary, after flushing any DATA
    /// result. A partial command cannot be replaced with a fabricated reply.
    pub fn service_unavailable(&mut self, clock: &(impl Clock + ?Sized)) -> Result<(), Error> {
        if !matches!(self.closing, Closing::Open) || self.work.is_some() {
            return Err(Error::Conflict);
        }
        let now = self.now(clock)?;
        self.queue_close(now)
    }

    fn queue_close(&mut self, now: Tick) -> Result<(), Error> {
        self.session.service_unavailable()?;
        self.closing = Closing::Reply {
            deadline: after(now, FINISH_SECONDS)?,
        };
        Ok(())
    }

    fn now(&mut self, clock: &(impl Clock + ?Sized)) -> Result<Tick, Error> {
        let result = clock.sample().and_then(|time| {
            if time.monotonic < self.last {
                Err(Error::Invalid)
            } else {
                Ok(time.monotonic)
            }
        });
        match result {
            Ok(now) => {
                self.last = now;
                Ok(now)
            }
            Err(error) => {
                self.session.abort();
                self.work = None;
                self.reply_grace = None;
                Err(error)
            }
        }
    }

    /// Every dispatched job retains this original enclosing deadline. The
    /// worker must additionally apply the operation's own execution budget.
    pub fn deadline(&self) -> Deadline {
        self.data_deadline
            .map_or(self.session_deadline, |d| d.min(self.session_deadline))
    }

    pub fn advance(
        &mut self,
        transport: &mut impl Transport,
        clock: &(impl Clock + ?Sized),
    ) -> Result<Progress, Error> {
        let result = self.turn(transport, clock);
        if result.is_err() {
            self.abort(transport);
        }
        result
    }

    pub fn abort(&mut self, transport: &mut impl Transport) {
        self.session.abort();
        transport.abort();
        self.start = 0;
        self.end = 0;
        self.work = None;
        self.reply_grace = None;
        self.closing = Closing::Closed;
    }

    fn turn(
        &mut self,
        transport: &mut impl Transport,
        clock: &(impl Clock + ?Sized),
    ) -> Result<Progress, Error> {
        if matches!(self.closing, Closing::Closed) {
            return Ok(Progress::Closed);
        }
        if matches!(self.closing, Closing::Open) && self.session.pending() == Pending::Closed {
            self.abort(transport);
            return Ok(Progress::Closed);
        }
        let now = self.now(clock)?;
        match self.closing {
            Closing::Shutdown { deadline } => {
                if deadline.expired(now) {
                    self.abort(transport);
                    return Ok(Progress::Closed);
                }
                return match transport.close()? {
                    FlushProgress::Complete => {
                        self.closing = Closing::Drain { deadline, bytes: 0 };
                        Ok(Progress::Advanced)
                    }
                    FlushProgress::Pending => Ok(Progress::Pending),
                };
            }
            Closing::Drain { deadline, bytes } => {
                if deadline.expired(now) || bytes == DRAIN_BYTES {
                    self.abort(transport);
                    return Ok(Progress::Closed);
                }
                let length = (DRAIN_BYTES - bytes).min(SOCKET_CHUNK);
                let output = self.input.get_mut(..length).ok_or(Error::Invalid)?;
                match transport.read(output)? {
                    IoProgress::Bytes(n) if n > 0 && n <= length => {
                        self.closing = Closing::Drain {
                            deadline,
                            bytes: bytes + n,
                        };
                    }
                    IoProgress::Pending => return Ok(Progress::Pending),
                    IoProgress::Closed => {
                        self.abort(transport);
                        return Ok(Progress::Closed);
                    }
                    _ => return Err(Error::Invalid),
                }
                return Ok(Progress::Advanced);
            }
            Closing::Reply { deadline } => {
                if deadline.expired(now) {
                    self.abort(transport);
                    return Ok(Progress::Closed);
                }
            }
            Closing::Open => {
                if let Some(deadline) = self.reply_grace {
                    if deadline.expired(now) {
                        return Err(Error::Deadline);
                    }
                } else if self.deadline().expired(now)
                    || (self.work.is_none() && self.idle_deadline.expired(now))
                {
                    if self.work.is_none() && self.queue_close(now).is_ok() {
                        return Ok(Progress::Advanced);
                    }
                    return Err(Error::Deadline);
                }
            }
            Closing::Closed => return Ok(Progress::Closed),
        }
        match self.session.pending() {
            Pending::Reply { bytes, close } => {
                if self.written < bytes.len() {
                    let input = bytes.get(self.written..).ok_or(Error::Invalid)?;
                    match transport.write(input)? {
                        IoProgress::Bytes(n) if n > 0 && n <= input.len() => {
                            self.written += n;
                            self.idle_deadline = after(now, self.idle_seconds)?;
                        }
                        IoProgress::Pending => return Ok(Progress::Pending),
                        _ => return Err(Error::Invalid),
                    }
                } else if self.written != bytes.len() {
                    return Err(Error::Invalid);
                } else if transport.flush()? == FlushProgress::Complete {
                    self.session.reply_sent()?;
                    self.written = 0;
                    let grace = self.reply_grace.take();
                    if close {
                        self.start = 0;
                        self.end = 0;
                        let deadline = match self.closing {
                            Closing::Reply { deadline } => deadline,
                            _ => grace.unwrap_or(after(now, FINISH_SECONDS)?),
                        };
                        self.closing = Closing::Shutdown { deadline };
                    } else if self.session_deadline.expired(now) {
                        self.session.service_unavailable()?;
                        self.closing = Closing::Reply {
                            deadline: grace.unwrap_or(after(now, FINISH_SECONDS)?),
                        };
                    }
                } else {
                    return Ok(Progress::Pending);
                }
            }
            Pending::Input => {
                if self.start != self.end {
                    let bytes = self.input.get(self.start..self.end).ok_or(Error::Invalid)?;
                    let n = self.session.feed(bytes)?;
                    if n == 0 || n > bytes.len() {
                        return Err(Error::Invalid);
                    }
                    self.start += n;
                } else {
                    match transport.read(&mut self.input)? {
                        IoProgress::Bytes(n) if n > 0 && n <= self.input.len() => {
                            self.start = 0;
                            self.end = n;
                            self.idle_deadline = after(now, self.idle_seconds)?;
                        }
                        IoProgress::Pending => return Ok(Progress::Pending),
                        IoProgress::Closed => {
                            self.abort(transport);
                            return Ok(Progress::Closed);
                        }
                        _ => return Err(Error::Invalid),
                    }
                }
            }
            Pending::BeginData { .. } => {
                if self.data_deadline.is_none() {
                    self.data_deadline = Some(after(now, self.data_seconds)?);
                }
                self.work = Some(Work::Begin);
                return Ok(Progress::Work);
            }
            Pending::Data(_) => {
                self.work = Some(Work::Data);
                return Ok(Progress::Work);
            }
            Pending::Commit => {
                self.work = Some(Work::Commit);
                return Ok(Progress::Work);
            }
            Pending::StartTls { .. } => {
                self.work = Some(Work::Tls);
                return Ok(Progress::Work);
            }
            Pending::Closed => {
                self.abort(transport);
                return Ok(Progress::Closed);
            }
        }
        // A completed or rejected DATA transaction drops its envelope. This
        // never renews the enclosing connection lifetime.
        if self.session.envelope().is_none() {
            self.data_deadline = None;
        }
        Ok(Progress::Advanced)
    }
}

fn after(now: Tick, seconds: u64) -> Result<Deadline, Error> {
    timers::deadline(now, seconds).map_err(|_| Error::Invalid)
}

#[cfg(test)]
#[path = "smtp_network_tests.rs"]
mod tests;
