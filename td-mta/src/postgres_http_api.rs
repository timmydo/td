//! Prepared PostgreSQL read API for the fixed HTTPS body worker.
use crate::{
    admission::timers::TimeoutPlan,
    clock::RuntimeClock,
    device_credentials::Presented,
    https_api::{self, ApiError, Exchange, Request},
    https_download::Failure,
    https_exchange::Sender,
    https_upload,
    ids::AccountId,
    jmap_request::dispatch,
    jmap_session::Session,
    limits::ResourcePlan,
    ports::{Error, Transport},
    postgres_input::Loaded,
    postgres_read::{self, ReadSlots, Snapshot},
    postgres_runtime_profile::Binding,
    smtp_receiving::Control,
    store_fs::ResponseCoordinator,
};
use std::path::Path;

/// Inert until the atomic backend cutover. The prepared session must belong
/// to this binding. HTTPS and response retention must share this exact clock.
/// Read capacity is reserved before fresh DNS; no SQL runs on the reactor.
pub struct Service<'s, 'sr, 'a> {
    pub loaded: &'s Loaded,
    pub executable: &'s Path,
    pub binding: &'s Binding,
    pub slots: &'s ReadSlots,
    pub responses: &'s ResponseCoordinator<'sr, 'a>,
    pub resources: &'s ResourcePlan,
    pub session: &'s Session,
    pub response_maximum: u64,
    pub clock: &'s RuntimeClock,
    pub timeouts: &'s TimeoutPlan,
}

impl https_upload::Backend for Service<'_, '_, '_> {
    fn account(&self) -> AccountId {
        self.binding.account()
    }
    fn session(&self) -> Option<&Session> {
        Some(self.session)
    }
    fn receive(
        &self,
        _request: https_upload::Request<'_>,
        _transport: &mut dyn Transport,
        _control: &Control,
    ) -> Result<https_upload::Response, Error> {
        Err(Error::NotFound)
    }
    fn api(
        &self,
        request: Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<(), Failure> {
        let mut sender = Sender::new(
            transport,
            control,
            self.clock,
            self.timeouts,
            request.deadline,
        )
        .map_err(Failure::Before)?;
        let result = self.execute(&request, &mut sender);
        let result = if self.responses.admission_stopped() {
            Err(Error::WriterStopped)
        } else {
            match result {
                Ok(()) => Ok(()),
                Err(ApiError::Request(error)) => https_api::problem(&mut sender, error),
                Err(ApiError::Io(error)) => Err(error),
            }
        };
        result.map_err(|error| {
            if sender.started {
                Failure::Started(error)
            } else {
                Failure::Before(error)
            }
        })
    }
}
impl Service<'_, '_, '_> {
    fn execute(&self, request: &Request<'_>, sender: &mut Sender<'_>) -> Result<(), ApiError> {
        let presented = Presented::parse(request.credentials.password()).ok_or(Error::Forbidden)?;
        if presented.id() != request.device {
            return Err(Error::Forbidden.into());
        }
        let exchange = Exchange {
            resources: self.resources,
            session: self.session,
            response_maximum: self.response_maximum,
            timeouts: self.timeouts,
        };
        let start = exchange.begin(request, sender)?;
        let mut retention = self
            .responses
            .retain_response(sender.deadline)
            .map_err(https_api::response_error)?;
        // The network worker authenticated the request. Full fresh authority
        // for reads is established below, after bounded input preparation.
        let (plan, execution) =
            exchange.receive(request, sender, start, &mut retention, |_| Ok(()))?;
        retention.admit_work_turn()?;
        let mut snapshot = Snapshot::open_resolved(
            self.slots,
            self.loaded,
            self.executable,
            self.clock.deadline_instant(execution)?,
            self.binding,
            request.credentials.password(),
        )
        .map_err(|error| ApiError::Io(read_error(error)))?;
        let complete =
            dispatch::run_with_postgres_read(plan, retention, execution, &mut snapshot, self.clock)
                .map_err(|error| {
                    ApiError::Io(match error {
                        dispatch::Error::Storage(error) => error,
                        dispatch::Error::ExecutionDeadline => Error::Deadline,
                        dispatch::Error::StorageBusy => Error::Busy,
                        dispatch::Error::Forbidden => Error::Forbidden,
                        _ => Error::Capacity,
                    })
                })?;
        // All response bytes are retained and the final snapshot check passed.
        // A slow client must not hold a database transaction or read slot.
        drop(snapshot);
        https_api::send_response(complete, sender)
    }
}
fn read_error(error: postgres_read::Error) -> Error {
    match error {
        postgres_read::Error::Connection(crate::postgres_probe::Error::Deadline) => Error::Deadline,
        postgres_read::Error::Busy => Error::Busy,
        postgres_read::Error::Unauthenticated | postgres_read::Error::Policy => Error::Forbidden,
        postgres_read::Error::Capacity => Error::Capacity,
        postgres_read::Error::Resolution(_) => Error::Dns,
        _ => Error::Invalid,
    }
}
