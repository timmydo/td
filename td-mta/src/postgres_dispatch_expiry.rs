//! Bounded expiry publication; clock confirmation and fencing remain mandatory.
use super::{expiry, ports, Deadline, Dispatches, Error, Meter, Operation, ATTEMPT_IO};

impl Dispatches<'_, '_> {
    /// Publish at most one expiry batch after the caller closes route transport.
    /// Unknown COMMIT retains the exact operation for receipt inspection only.
    pub fn expire(
        &self,
        value: expiry::Expiry,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<expiry::Outcome, Error> {
        let mut pending = self
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        let limit = self.connection.admit(until, meter, ATTEMPT_IO)?;
        let _slot = self
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        *pending = Some(Operation::Expiry(Box::new(value)));
        let Some(Operation::Expiry(value)) = pending.as_ref() else {
            return Err(ports::Error::Invalid.into());
        };
        match expiry::publish(
            self.connection.request,
            self.connection.trust,
            limit,
            value,
            self.connection.clock.clone(),
            self.guard.clone(),
        ) {
            Ok(outcome) => {
                *pending = None;
                Ok(outcome)
            }
            Err(error) => {
                if error != expiry::Error::Indeterminate {
                    *pending = None;
                }
                Err(Error::Expiry(error))
            }
        }
    }
}
