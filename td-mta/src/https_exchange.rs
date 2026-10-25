//! Fixed-worker HTTPS output under one original deadline and idle budget.
use crate::{
    admission::timers::{self, TimeoutPlan},
    ports::{Clock, Deadline, Error, FlushProgress, IoProgress, Tick, Transport},
    smtp_receiving::Control,
};
use std::time::Duration;

pub(crate) struct Sender<'a> {
    pub(crate) transport: &'a mut dyn Transport,
    pub(crate) control: &'a Control,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) timeouts: &'a TimeoutPlan,
    pub(crate) deadline: Deadline,
    pub(crate) idle: Deadline,
    pub(crate) last: Tick,
    pub(crate) started: bool,
    pub(crate) maintenance: Option<&'a dyn Fn()>,
}
impl<'a> Sender<'a> {
    pub(crate) fn new(
        transport: &'a mut dyn Transport,
        control: &'a Control,
        clock: &'a dyn Clock,
        timeouts: &'a TimeoutPlan,
        deadline: Deadline,
    ) -> Result<Self, Error> {
        let last = clock.sample()?.monotonic;
        let idle = deadline;
        Ok(Self {
            transport,
            control,
            clock,
            timeouts,
            deadline,
            idle,
            last,
            started: false,
            maintenance: None,
        })
    }
    pub(crate) fn check(&mut self) -> Result<(), Error> {
        if let Some(maintain) = self.maintenance {
            maintain();
        }
        let now = self.clock.sample()?.monotonic;
        if now < self.last {
            return Err(Error::WriterStopped);
        }
        self.last = now;
        if self.control.stop_requested() || self.deadline.expired(now) || self.idle.expired(now) {
            return Err(Error::Deadline);
        }
        Ok(())
    }
    pub(crate) fn write(&mut self, mut bytes: &[u8]) -> Result<(), Error> {
        while !bytes.is_empty() {
            self.check()?;
            self.started = true;
            match self.transport.write(bytes)? {
                IoProgress::Bytes(n) if n > 0 && n <= bytes.len() => {
                    self.check()?;
                    bytes = bytes.get(n..).ok_or(Error::Invalid)?;
                    self.idle =
                        timers::deadline(self.last, self.timeouts.limits().response_idle_seconds)
                            .map_err(|_| Error::Invalid)?
                            .min(self.deadline);
                }
                IoProgress::Pending => std::thread::sleep(Duration::from_millis(5)),
                _ => return Err(Error::Invalid),
            }
        }
        Ok(())
    }
    pub(crate) fn flush(&mut self) -> Result<(), Error> {
        loop {
            self.check()?;
            if self.transport.flush()? == FlushProgress::Complete {
                return self.check();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
