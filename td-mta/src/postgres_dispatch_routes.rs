//! Bounded route-claim publication; a receipt is never live SMTP authority.
use super::{owners, ports, Claim, Deadline, Dispatches, Error, Meter, Operation, ATTEMPT_IO};

impl Dispatches<'_, '_> {
    /// Acquire or recover this exact claim under the worker's admission bounds.
    /// Retain a clone for later current-owner checks or definite-failure retry.
    /// Fresh current-owner/attempt checks remain mandatory before transport.
    /// Unknown COMMIT retains the claim in Pending for receipt inspection only.
    pub fn acquire_route(
        &self,
        claim: Claim,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<owners::Grant, Error> {
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
        *pending = Some(Operation::RouteClaim(Box::new(claim)));
        let Some(Operation::RouteClaim(claim)) = pending.as_ref() else {
            return Err(ports::Error::Invalid.into());
        };
        match owners::acquire(self.connection.request, self.connection.trust, limit, claim) {
            Ok(grant) => {
                *pending = None;
                Ok(grant)
            }
            Err(error) => {
                if error != owners::Error::Indeterminate {
                    *pending = None;
                }
                Err(Error::Owner(error))
            }
        }
    }
}
