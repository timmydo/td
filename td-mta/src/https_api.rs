//! Authenticated Core API work on the fixed HTTPS body worker.
use crate::{
    admission::timers::{self, TimeoutPlan},
    bounded::TextBuffer,
    http_auth::DevicePolicy,
    http_request::{self, BodyReader, Framing, QUANTUM},
    https_download::Failure,
    https_exchange::Sender,
    https_upload,
    ids::{AccountId, DeviceId},
    jmap_request::{self, dispatch, response},
    jmap_session::Session,
    limits::ResourcePlan,
    ports::{Clock, Deadline, Error, IoProgress, Transport},
    smtp_receiving::Control,
    store_fs::{BlobAuthorization, RequestSpool, ResponseError, StoreCoordinator},
};
use std::{thread, time::Duration};
use td_json::arena;

pub struct Request<'a> {
    pub device: DeviceId,
    pub framing: Framing,
    pub prefix: &'a [u8],
    pub expect_continue: bool,
    pub deadline: Deadline,
}

pub struct Service<'s, 'r, 'a, 'sr, 'd, P> {
    pub transfers: &'s dyn https_upload::Backend,
    pub coordinator: &'s StoreCoordinator<'r, 'a, P>,
    pub spool: &'s RequestSpool<'sr>,
    pub policy: DevicePolicy<'s, 'd>,
    pub resources: &'s ResourcePlan,
    pub session: &'s Session,
    pub response_maximum: u64,
    pub clock: &'s dyn Clock,
    pub timeouts: &'s TimeoutPlan,
}

#[derive(Debug)]
enum ApiError {
    Request(jmap_request::Error),
    Io(Error),
}
impl From<Error> for ApiError {
    fn from(value: Error) -> Self {
        Self::Io(value)
    }
}
fn parser(error: arena::Error) -> ApiError {
    ApiError::Request(match error.kind {
        arena::ErrorKind::Syntax => jmap_request::Error::NotJson,
        arena::ErrorKind::Limit(arena::ParseLimit::Bytes) => jmap_request::Error::MaxSizeRequest,
        arena::ErrorKind::Limit(kind) => jmap_request::Error::Resource(kind),
        arena::ErrorKind::Storage => jmap_request::Error::Storage,
    })
}
fn response_error(error: ResponseError) -> ApiError {
    ApiError::Io(match error {
        ResponseError::Store(error) => error,
        ResponseError::Ledger(_) => Error::Busy,
    })
}
fn wire_error(error: jmap_request::wire::Error) -> ApiError {
    ApiError::Io(match error {
        jmap_request::wire::Error::Storage(error) => error,
        _ => Error::Capacity,
    })
}
fn body_error(error: http_request::Error) -> ApiError {
    match error {
        http_request::Error::Limit => ApiError::Request(jmap_request::Error::MaxSizeRequest),
        _ => ApiError::Io(Error::Invalid),
    }
}
fn buffer<T: Clone>(length: usize, value: T) -> Result<Box<[T]>, Error> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Error::Capacity)?;
    bytes.resize(length, value);
    Ok(bytes.into_boxed_slice())
}
impl<P: Sync> https_upload::Backend for Service<'_, '_, '_, '_, '_, P> {
    fn account(&self) -> AccountId {
        self.transfers.account()
    }
    fn receive(
        &self,
        request: https_upload::Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<https_upload::Response, Error> {
        self.transfers.receive(request, transport, control)
    }
    fn download(
        &self,
        request: crate::https_download::Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<(), Failure> {
        self.transfers.download(request, transport, control)
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
        // All temporary response owners have now dropped and settled their ledger.
        if self.coordinator.admission_stopped() {
            return Err(if sender.started {
                Failure::Started(Error::WriterStopped)
            } else {
                Failure::Before(Error::WriterStopped)
            });
        }
        let result = match result {
            Ok(()) => Ok(()),
            Err(ApiError::Request(error)) => problem(&mut sender, error),
            Err(ApiError::Io(error)) => Err(error),
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
impl<P> Service<'_, '_, '_, '_, '_, P> {
    fn execute(&self, request: &Request<'_>, sender: &mut Sender<'_>) -> Result<(), ApiError> {
        let limits = self.resources.limits();
        let device = request.device;
        let start = sender.last;
        sender.deadline = timers::deadline(
            start,
            self.timeouts
                .http_exchange_seconds(limits.json_bytes as u64, self.response_maximum)
                .map_err(|_| Error::Capacity)?,
        )
        .map_err(|_| Error::Capacity)?
        .min(request.deadline);
        sender.idle = sender.deadline;
        sender.check()?;
        jmap_request::Request::validate_resources(self.resources).map_err(ApiError::Request)?;
        let mut retention = self
            .coordinator
            .retain_response(self.spool, sender.deadline)
            .map_err(response_error)?;
        if retention.maximum() != self.response_maximum {
            return Err(Error::Capacity.into());
        }
        let wire_max = (limits.json_bytes as u64)
            .checked_mul(8)
            .and_then(|n| n.checked_add(65536))
            .ok_or(Error::Capacity)?;
        let mut body = BodyReader::new(request.framing, limits.json_bytes as u64, wire_max)
            .map_err(body_error)?;
        let mut bytes = buffer(limits.json_bytes, 0u8)?;
        let tokens = buffer(limits.json_tokens, arena::Token::EMPTY)?;
        let transfer = timers::deadline(
            start,
            self.timeouts
                .http_transfer_seconds(limits.json_bytes as u64)
                .map_err(|_| Error::Capacity)?,
        )
        .map_err(|_| Error::Capacity)?
        .min(sender.deadline);
        if request.expect_continue {
            sender.idle =
                timers::deadline(sender.last, self.timeouts.limits().response_idle_seconds)
                    .map_err(|_| Error::Capacity)?
                    .min(transfer);
            sender.write(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            sender.flush()?;
            // A fully flushed interim response permits one subsequent final response.
            sender.started = false;
        }
        sender.idle = timers::deadline(sender.last, self.timeouts.limits().body_idle_seconds)
            .map_err(|_| Error::Capacity)?
            .min(transfer);
        let mut input = [0; QUANTUM];
        let mut output = [0; QUANTUM];
        let mut used = request.prefix.len();
        input
            .get_mut(..used)
            .ok_or(Error::Invalid)?
            .copy_from_slice(request.prefix);
        let mut length = 0usize;
        loop {
            sender.check()?;
            let next = body
                .feed(input.get(..used).ok_or(Error::Invalid)?, &mut output)
                .map_err(body_error)?;
            let end = length.checked_add(next.written).ok_or(Error::Capacity)?;
            bytes
                .get_mut(length..end)
                .ok_or(Error::Capacity)?
                .copy_from_slice(output.get(..next.written).ok_or(Error::Invalid)?);
            length = end;
            if next.complete {
                break;
            }
            if next.consumed != used {
                return Err(Error::Invalid.into());
            }
            used = match sender.transport.read(&mut input)? {
                IoProgress::Bytes(n) if n > 0 && n <= input.len() => {
                    sender.check()?;
                    sender.idle =
                        timers::deadline(sender.last, self.timeouts.limits().body_idle_seconds)
                            .map_err(|_| Error::Capacity)?
                            .min(transfer);
                    n
                }
                IoProgress::Pending => {
                    thread::sleep(Duration::from_millis(5));
                    0
                }
                _ => return Err(Error::Invalid.into()),
            };
        }
        sender.check()?;
        sender.idle = sender.deadline;
        drop(self.policy.authorize_blob(
            self.transfers.account(),
            request.device,
            sender.deadline,
        )?);
        let execution = timers::deadline(sender.last, self.timeouts.execution_seconds())
            .map_err(|_| Error::Capacity)?
            .min(sender.deadline);
        let mut cursor = arena::OwnedCursor::with_input_length(
            bytes,
            tokens,
            length,
            arena::ParseLimits {
                max_bytes: limits.json_bytes,
                max_depth: limits.json_depth,
                max_tokens: limits.json_tokens,
            },
        )
        .map_err(|error| parser(error.error()))?;
        loop {
            sender.check()?;
            if !retention.admit_before(execution)? {
                return Err(Error::Deadline.into());
            }
            if cursor.step().map_err(parser)? {
                break;
            }
        }
        sender.check()?;
        let document = cursor.finish().map_err(|error| parser(error.error()))?;
        let mut validation =
            jmap_request::OwnedValidation::new(document, self.resources, &[jmap_request::CORE])
                .map_err(|error| ApiError::Request(error.error()))?;
        loop {
            sender.check()?;
            if !retention.admit_before(execution)? {
                return Err(Error::Deadline.into());
            }
            if validation.step().map_err(ApiError::Request)? {
                break;
            }
        }
        sender.check()?;
        let validated_request = validation
            .finish()
            .map_err(|error| ApiError::Request(error.error()))?;
        let mut plan = response::Preflight::new(
            validated_request,
            self.session.state()?,
            retention.maximum(),
        )
        .map_err(|_| Error::Capacity)?;
        loop {
            sender.check()?;
            if !retention.admit_before(execution)? {
                return Err(Error::Deadline.into());
            }
            if plan.step(&mut output).map_err(|_| Error::Capacity)? {
                break;
            }
        }
        sender.check()?;
        retention.admit_work_turn()?;
        drop(
            self.policy
                .authorize_blob(self.transfers.account(), device, sender.deadline)?,
        );
        let mut complete = dispatch::run(
            plan.finish().map_err(|_| Error::Capacity)?,
            retention,
            execution,
            self.transfers.account(),
        )
        .map_err(|error| {
            ApiError::Io(match error {
                dispatch::Error::Storage(error) => error,
                _ => Error::Capacity,
            })
        })?;
        let mut encoder = complete.encoder().map_err(wire_error)?;
        loop {
            sender.check()?;
            if encoder.count_step(&mut output).map_err(wire_error)? {
                break;
            }
        }
        let length = encoder.begin_output().map_err(wire_error)?;
        sender.check()?;
        sender.idle = timers::deadline(sender.last, self.timeouts.limits().response_idle_seconds)
            .map_err(|_| Error::Capacity)?
            .min(sender.deadline);
        headers(sender, 200, "application/json", length)?;
        loop {
            sender.check()?;
            let next = encoder.poll_output(&mut output).map_err(wire_error)?;
            sender.write(output.get(..next.written).ok_or(Error::Invalid)?)?;
            if next.complete {
                break;
            }
        }
        encoder.finish().map_err(wire_error)?;
        sender.flush()?;
        drop(complete);
        Ok(())
    }
}
fn headers(sender: &mut Sender<'_>, status: u16, media: &str, length: u64) -> Result<(), Error> {
    let mut bytes = [0; 1024];
    let mut out = TextBuffer::new(&mut bytes);
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Service Unavailable",
    };
    out.format(format_args!(
        concat!(
            "HTTP/1.1 {} {}\r\nDate: {}\r\nConnection: close\r\n",
            "Content-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n"
        ),
        status,
        reason,
        crate::https::http_date(sender.clock.sample()?.utc_ms)?,
        media,
        length,
    ))
    .map_err(|_| Error::Capacity)?;
    let length = out.len();
    sender.write(bytes.get(..length).ok_or(Error::Invalid)?)
}
fn problem(sender: &mut Sender<'_>, error: jmap_request::Error) -> Result<(), Error> {
    let body: &[u8] = match error {
        jmap_request::Error::NotJson => br#"{"type":"urn:ietf:params:jmap:error:notJSON","status":400}"#,
        jmap_request::Error::NotRequest => br#"{"type":"urn:ietf:params:jmap:error:notRequest","status":400}"#,
        jmap_request::Error::UnknownCapability => br#"{"type":"urn:ietf:params:jmap:error:unknownCapability","status":400}"#,
        jmap_request::Error::MaxSizeRequest => br#"{"type":"urn:ietf:params:jmap:error:limit","status":400,"limit":"maxSizeRequest"}"#,
        jmap_request::Error::MaxCallsInRequest => br#"{"type":"urn:ietf:params:jmap:error:limit","status":400,"limit":"maxCallsInRequest"}"#,
        jmap_request::Error::Configuration | jmap_request::Error::Storage
        | jmap_request::Error::Resource(_) => return Err(Error::Capacity),
    };
    if sender.started {
        return Err(Error::Invalid);
    }
    sender.check()?;
    sender.idle = timers::deadline(sender.last, sender.timeouts.limits().response_idle_seconds)
        .map_err(|_| Error::Capacity)?
        .min(sender.deadline);
    headers(sender, 400, "application/problem+json", body.len() as u64)?;
    sender.write(body)?;
    sender.flush()
}
