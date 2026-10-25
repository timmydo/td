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
use std::cell::RefCell;
use std::path::Path;
use std::time::Instant;

struct CachedRead<'a> {
    session: Option<crate::postgres_probe::ReadSession>,
    _permit: crate::postgres_read::ReadPermit<'a>,
}
impl Drop for CachedRead<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            session.retire(Instant::now() + std::time::Duration::from_millis(100));
        }
    }
}
struct ApiWorker<'w, 's, 'sr, 'a> {
    service: &'w Service<'s, 'sr, 'a>,
    cached: RefCell<Option<CachedRead<'s>>>,
}
fn retire_cached(cache: &RefCell<Option<CachedRead<'_>>>) {
    let Ok(mut cache) = cache.try_borrow_mut() else {
        return;
    };
    if cache
        .as_ref()
        .and_then(|cached| cached.session.as_ref())
        .is_some_and(|session| session.needs_retirement(Instant::now()))
    {
        *cache = None;
    }
}
impl https_api::Worker for ApiWorker<'_, '_, '_, '_> {
    fn maintain(&mut self) -> Result<(), Error> {
        retire_cached(&self.cached);
        Ok(())
    }
    fn api(
        &mut self,
        request: Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<(), Failure> {
        self.service
            .serve_api(request, transport, control, Some(&self.cached))
    }
}

/// The prepared session must belong
/// to this binding. HTTPS and response retention must share this exact clock.
/// Read capacity is reserved before fresh DNS; no SQL runs on the reactor.
pub struct Service<'s, 'sr, 'a> {
    pub loaded: &'s Loaded,
    pub executable: &'s Path,
    pub binding: &'s Binding,
    pub slots: &'s ReadSlots,
    pub pending: &'s crate::postgres_http_updates::Pending,
    pub responses: &'s ResponseCoordinator<'sr, 'a>,
    pub resources: &'s ResourcePlan,
    pub session: &'s Session,
    pub response_maximum: u64,
    pub clock: &'s RuntimeClock,
    pub admission: &'s crate::admission::Plan,
    pub timeouts: &'s TimeoutPlan,
}

impl https_upload::Backend for Service<'_, '_, '_> {
    fn account(&self) -> AccountId {
        self.binding.account()
    }
    fn api_worker(&self) -> Option<Box<dyn https_api::Worker + '_>> {
        Some(Box::new(ApiWorker {
            service: self,
            cached: RefCell::new(None),
        }))
    }
    fn session(&self) -> Option<&Session> {
        Some(self.session)
    }
    fn receive(
        &self,
        request: https_upload::Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<https_upload::Response, Error> {
        crate::postgres_http_upload_receive::receive(self, request, transport, control)
    }
    fn download(
        &self,
        request: crate::https_download::Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<(), Failure> {
        self.send_download(request, transport, control)
    }
    fn api(
        &self,
        request: Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<(), Failure> {
        self.serve_api(request, transport, control, None)
    }
}
impl<'s, 'sr, 'a> Service<'s, 'sr, 'a> {
    fn read_cached<T>(
        &self,
        cache: &RefCell<Option<CachedRead<'s>>>,
        deadline: Instant,
        password: &[u8],
        read: impl FnOnce(&mut Snapshot<'_>) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        use crate::postgres_probe::{self, ReadIsolation, ReadSession};
        if !self.executable.is_absolute()
            || self.loaded.bootstrap.profile() != self.binding.profile()
            || !self.binding.roles().jmap()
        {
            return Err(Error::Forbidden.into());
        }
        let presented = Presented::parse(password).ok_or(Error::Forbidden)?;
        let device = presented.id();
        let mut cache = cache.try_borrow_mut().map_err(|_| Error::Busy)?;
        let probe_error = |error| ApiError::Io(read_error(postgres_read::Error::Connection(error)));
        if let Some(session) = cache.as_mut().and_then(|cached| cached.session.as_mut()) {
            if !session.reusable(deadline) || session.check_idle(deadline).is_err() {
                *cache = None;
            }
        }
        let reused = cache.is_some();
        if cache.is_none() {
            let permit = self
                .slots
                .acquire()
                .map_err(|error| ApiError::Io(read_error(error)))?;
            let dns_deadline = deadline.min(Instant::now() + std::time::Duration::from_secs(5));
            let addresses = crate::postgres_resolver::resolve(
                self.executable,
                &self.loaded.bootstrap,
                dns_deadline,
            )
            .map_err(|error| ApiError::Io(read_error(postgres_read::Error::Resolution(error))))?;
            let mut opened = None;
            for address in addresses {
                let mut request = self.loaded.bootstrap.request(address);
                request.mode = postgres_probe::Mode::Runtime;
                match ReadSession::open_body(&request, &self.loaded.trust, deadline) {
                    Ok(session) => {
                        opened = Some(session);
                        break;
                    }
                    Err(postgres_probe::Error::Transport) => (),
                    Err(error) => return Err(probe_error(error)),
                }
            }
            let session = opened.ok_or_else(|| probe_error(postgres_probe::Error::Transport))?;
            *cache = Some(CachedRead {
                session: Some(session),
                _permit: permit,
            });
        }
        let session = cache
            .as_mut()
            .and_then(|cached| cached.session.as_mut())
            .ok_or(Error::Invalid)?;
        let result = session
            .read_worker(
                self.binding.epoch(),
                ReadIsolation::RepeatableReadOnly,
                deadline,
                |worker| {
                    let binding = self.binding.clone();
                    let identity = worker.run(|client| {
                        Box::pin(async move {
                            crate::postgres_snapshot::observe(client, &binding, presented).await
                        })
                    })?;
                    let identity = match identity {
                        Ok(identity) => identity,
                        Err(error) => {
                            return Ok(Err(ApiError::Io(read_error(
                                postgres_read::snapshot_error(error),
                            ))))
                        }
                    };
                    Ok((|| {
                        let mut snapshot =
                            Snapshot::borrowed(worker, identity, deadline, self.binding, device)
                                .map_err(|error| ApiError::Io(read_error(error)))?;
                        read(&mut snapshot)
                    })())
                },
            )
            .map_err(probe_error)
            .and_then(|result| result);
        if reused {
            crate::performance::record(
                crate::performance::Phase::DbReuse,
                std::time::Duration::ZERO,
                if result.is_ok() {
                    crate::performance::Outcome::Success
                } else {
                    crate::performance::Outcome::Failed
                },
            );
        }
        if result.is_err() {
            *cache = None;
        }
        result
    }
    fn serve_api(
        &self,
        request: Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
        cache: Option<&RefCell<Option<CachedRead<'s>>>>,
    ) -> Result<(), Failure> {
        let maintenance = || {
            if let Some(cache) = cache {
                retire_cached(cache);
            }
        };
        let mut sender = Sender::new(
            transport,
            control,
            self.clock,
            self.timeouts,
            request.deadline,
        )
        .map_err(Failure::Before)?;
        sender.maintenance = Some(&maintenance);
        let result = self.execute(&request, &mut sender, cache);
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
    fn send_download(
        &self,
        request: crate::https_download::Request<'_>,
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
        let result = self.download_body(&request, &mut sender);
        result.map_err(|error| {
            if sender.started {
                Failure::Started(error)
            } else {
                Failure::Before(error)
            }
        })
    }
    fn download_body(
        &self,
        request: &crate::https_download::Request<'_>,
        sender: &mut Sender<'_>,
    ) -> Result<(), Error> {
        use crate::{
            admission::work::{Charge, Meter},
            ports::Deadline,
            postgres_body, postgres_probe,
        };
        if request.target.account() != self.binding.account() {
            return Err(Error::NotFound);
        }
        if !self.binding.roles().jmap()
            || self.loaded.bootstrap.profile() != self.binding.profile()
            || !self.executable.is_absolute()
        {
            return Err(Error::Forbidden);
        }
        let presented = Presented::parse(request.credentials.password()).ok_or(Error::Forbidden)?;
        if presented.id() != request.device {
            return Err(Error::Forbidden);
        }
        sender.check()?;
        let started_at = sender.last;
        let until = Deadline::after(started_at, 30_000)?.min(request.deadline);
        sender.deadline = until;
        let deadline = self.clock.deadline_instant(until)?;
        // Verification retains one snapshot while fresh authorization opens
        // another connection. Reserve both before launching the DNS helper.
        let _body_slot = self.slots.acquire().map_err(read_error)?;
        let _auth_slot = self.slots.acquire().map_err(read_error)?;
        let dns_deadline = deadline.min(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(5))
                .ok_or(Error::Invalid)?,
        );
        let addresses = crate::postgres_resolver::resolve(
            self.executable,
            &self.loaded.bootstrap,
            dns_deadline,
        )
        .map_err(|error| match error {
            crate::postgres_resolver::Error::Deadline => Error::Deadline,
            _ => Error::Dns,
        })?;
        let work = self.admission.work();
        let mut meter = Meter::new(
            until,
            Charge {
                io_bytes: work.foreground_io_bytes,
                records: work.foreground_records,
                output_bytes: work.foreground_io_bytes,
                ..Default::default()
            },
        );
        for address in addresses {
            let mut connection = self.loaded.bootstrap.request(address);
            connection.mode = postgres_probe::Mode::Runtime;
            let result = postgres_body::with_pinned_blob(
                &connection,
                &self.loaded.trust,
                deadline,
                postgres_body::BlobRequest {
                    binding: self.binding,
                    blob: request.target.blob(),
                    maximum: postgres_body::MAX_BODY_BYTES,
                },
                request.credentials.password(),
                |_, pin| {
                    crate::https_download::send_verified(
                        pin,
                        &mut meter,
                        request.target,
                        self.resources.limits(),
                        started_at,
                        until,
                        sender,
                    )
                },
            );
            if std::time::Instant::now() >= deadline {
                return Err(Error::Deadline);
            }
            match result {
                Ok(()) => return Ok(()),
                Err(postgres_body::Error::Connection(postgres_probe::Error::Transport))
                    if !sender.started => {}
                Err(error) => {
                    return Err(match error {
                        postgres_body::Error::Connection(postgres_probe::Error::Deadline) => {
                            Error::Deadline
                        }
                        postgres_body::Error::Read(error) => error,
                        postgres_body::Error::Missing => Error::NotFound,
                        postgres_body::Error::Unauthenticated | postgres_body::Error::Policy => {
                            Error::Forbidden
                        }
                        postgres_body::Error::Capacity => Error::Capacity,
                        postgres_body::Error::Integrity => Error::Corrupt,
                        _ => Error::Invalid,
                    })
                }
            }
        }
        Err(Error::Invalid)
    }
    fn execute(
        &self,
        request: &Request<'_>,
        sender: &mut Sender<'_>,
        cache: Option<&RefCell<Option<CachedRead<'s>>>>,
    ) -> Result<(), ApiError> {
        let presented = Presented::parse(request.credentials.password()).ok_or(Error::Forbidden)?;
        if presented.id() != request.device {
            return Err(Error::Forbidden.into());
        }
        let exchange = Exchange {
            capabilities: self.session.capabilities(),
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
        let execution = execution.min(crate::ports::Deadline::after(sender.last, 10_000)?);
        let (mutation, send_batch) = {
            let request = plan.request().map_err(|_| ApiError::Io(Error::Invalid))?;
            let mut calls = request.calls();
            let first = calls.next();
            if request.call_count() == 1 {
                (first.map(|call| call.name), false)
            } else if request.call_count() == 2
                && first.is_some_and(|call| call.name == b"Email/set")
                && calls
                    .next()
                    .is_some_and(|call| call.name == b"EmailSubmission/set")
            {
                (Some(b"EmailSubmission/set".as_slice()), true)
            } else {
                (None, false)
            }
        };
        if let Some(cache) = cache {
            if !matches!(
                mutation,
                Some(b"Email/set" | b"Mailbox/set" | b"EmailSubmission/set")
            ) {
                let complete = self.read_cached(
                    cache,
                    self.clock.deadline_instant(execution)?,
                    request.credentials.password(),
                    |snapshot| {
                        dispatch::run_with_postgres_read(
                            plan, retention, execution, snapshot, self.clock, true,
                        )
                        .map_err(dispatch_error)
                    },
                )?;
                return https_api::send_response(complete, sender);
            }
            *cache.try_borrow_mut().map_err(|_| Error::Busy)? = None;
        }
        let mut snapshot = Snapshot::open_resolved(
            self.slots,
            self.loaded,
            self.executable,
            self.clock.deadline_instant(execution)?,
            self.binding,
            request.credentials.password(),
        )
        .map_err(|error| ApiError::Io(read_error(error)))?;
        let identity = snapshot.identity();
        let complete = if matches!(
            mutation,
            Some(b"Email/set" | b"Mailbox/set" | b"EmailSubmission/set")
        ) {
            drop(
                snapshot
                    .finish_method()
                    .map_err(|error| ApiError::Io(read_error(error)))?,
            );
            if mutation == Some(b"Mailbox/set".as_slice()) {
                let mailboxes = crate::postgres_http_mailboxes::Mailboxes {
                    loaded: self.loaded,
                    executable: self.executable,
                    binding: self.binding,
                    slots: self.slots,
                    clock: self.clock,
                    pending: &self.pending.mailboxes,
                    password: request.credentials.password(),
                };
                dispatch::run_with_postgres_mailboxes(
                    plan,
                    retention,
                    execution,
                    self.binding.account(),
                    request.device,
                    &mailboxes,
                )
            } else {
                let updates = crate::postgres_http_updates::Updates {
                    loaded: self.loaded,
                    executable: self.executable,
                    binding: self.binding,
                    slots: self.slots,
                    clock: self.clock,
                    pending: self.pending,
                    password: request.credentials.password(),
                };
                let drafts = crate::postgres_http_drafts::Drafts {
                    loaded: self.loaded,
                    executable: self.executable,
                    binding: self.binding,
                    slots: self.slots,
                    clock: self.clock,
                    pending: &self.pending.drafts,
                    password: request.credentials.password(),
                    maximum_text: self.resources.limits().json_bytes,
                };
                if mutation == Some(b"EmailSubmission/set".as_slice()) {
                    let submissions = crate::postgres_http_submissions::Submissions {
                        loaded: self.loaded,
                        executable: self.executable,
                        binding: self.binding,
                        slots: self.slots,
                        clock: self.clock,
                        pending: &self.pending.submissions,
                        password: request.credentials.password(),
                    };
                    let cancellations = crate::postgres_http_cancellations::Cancellations {
                        loaded: self.loaded,
                        executable: self.executable,
                        binding: self.binding,
                        slots: self.slots,
                        clock: self.clock,
                        pending: &self.pending.cancellations,
                        password: request.credentials.password(),
                    };
                    let access = dispatch::SubmissionAccess {
                        creator: &submissions,
                        canceler: Some(&cancellations),
                        filer: &updates,
                        clock: self.clock,
                        device: request.device,
                        state: crate::sync::DataState {
                            account: identity.account,
                            epoch: identity.epoch,
                            kind: crate::sync::DataType::Submission,
                            sequence: identity.committed_sequence,
                        },
                    };
                    if send_batch {
                        dispatch::run_with_postgres_send(
                            plan, retention, execution, access, &drafts, &updates,
                        )
                    } else {
                        dispatch::run_with_postgres_submissions(plan, retention, execution, access)
                    }
                } else {
                    dispatch::run_with_postgres_mail_access(
                        plan,
                        retention,
                        execution,
                        crate::sync::DataState {
                            account: identity.account,
                            epoch: identity.epoch,
                            kind: crate::sync::DataType::Email,
                            sequence: identity.committed_sequence,
                        },
                        request.device,
                        &drafts,
                        &updates,
                    )
                }
            }
        } else {
            let result = dispatch::run_with_postgres_read(
                plan,
                retention,
                execution,
                &mut snapshot,
                self.clock,
                true,
            );
            drop(snapshot);
            result
        }
        .map_err(dispatch_error)?;
        https_api::send_response(complete, sender)
    }
}
fn dispatch_error(error: dispatch::Error) -> ApiError {
    ApiError::Io(match error {
        dispatch::Error::Storage(error) => error,
        dispatch::Error::ExecutionDeadline => Error::Deadline,
        dispatch::Error::StorageBusy => Error::Busy,
        dispatch::Error::Forbidden => Error::Forbidden,
        _ => Error::Capacity,
    })
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
