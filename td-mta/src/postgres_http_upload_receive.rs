//! Bounded completed-ingress upload handoff on the fixed HTTPS body worker.
use crate::{
    admission::{
        timers,
        work::{Charge, Meter, Stop},
    },
    device_credentials::Presented,
    http_request::{BodyReader, Framing, QUANTUM},
    https_exchange::Sender,
    https_upload::{Request, Response},
    ports::{Clock, Error, IoProgress, Transport},
    postgres_http_api::Service,
    postgres_http_uploads::{self as owner, Input, Uploads},
    postgres_probe, postgres_uploads as changes,
    smtp_receiving::Control,
};
use std::time::Duration;

pub(crate) fn receive(
    service: &Service<'_, '_, '_>,
    request: Request<'_>,
    transport: &mut dyn Transport,
    control: &Control,
) -> Result<Response, Error> {
    if request.account != service.binding.account()
        || !service.binding.roles().jmap()
        || service.loaded.bootstrap.profile() != service.binding.profile()
        || !service.executable.is_absolute()
        || Presented::parse(request.credentials.password()).is_none_or(|p| p.id() != request.device)
    {
        return Err(Error::Forbidden);
    }
    if request.content_type.len() > 1024 {
        return Err(Error::Capacity);
    }
    if request.content_type.is_empty()
        || !request
            .content_type
            .bytes()
            .all(|b| b == b'\t' || (32..=126).contains(&b))
    {
        return Err(Error::Invalid);
    }
    let maximum = service
        .binding
        .upload_maximum()
        .min(service.resources.limits().message_bytes)
        .min(changes::MAX_BYTES) as u64;
    let maximum = match request.framing {
        Framing::Length(n) if n <= maximum => n,
        Framing::Length(_) => return Err(Error::Capacity),
        Framing::Chunked => maximum,
    };
    let wire_maximum = maximum
        .checked_mul(8)
        .and_then(|n| n.checked_add(65536))
        .ok_or(Error::Capacity)?;
    let mut reader = BodyReader::new(request.framing, maximum, wire_maximum).map_err(body_error)?;
    let body_deadline = request.body_deadline.min(request.deadline);
    if control.stop_requested() || body_deadline.expired(service.clock.sample()?.monotonic) {
        return Err(Error::Deadline);
    }
    let ingress = service.pending.uploads.admit_ingress()?;
    let work = service.admission.work();
    let output = maximum
        .checked_mul(2)
        .and_then(|n| n.checked_add(4096))
        .ok_or(Error::Capacity)?;
    let mut meter = Meter::new(
        request.deadline,
        Charge {
            io_bytes: work.foreground_io_bytes,
            records: work.foreground_records,
            output_bytes: output,
            ..Charge::default()
        },
    );
    charge(
        service,
        &mut meter,
        0,
        maximum.checked_add(4096).ok_or(Error::Capacity)?,
    )?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(maximum).map_err(|_| Error::Capacity)?)
        .map_err(|_| Error::Busy)?;
    let mut sender = Sender::new(
        transport,
        control,
        service.clock,
        service.timeouts,
        body_deadline,
    )?;
    sender.check()?;
    if request.expect_continue {
        sender.idle =
            timers::deadline(sender.last, service.timeouts.limits().response_idle_seconds)
                .map_err(|_| Error::Invalid)?
                .min(body_deadline);
        sender.write(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        sender.flush()?;
    }
    sender.check()?;
    sender.idle = timers::deadline(sender.last, service.timeouts.limits().body_idle_seconds)
        .map_err(|_| Error::Invalid)?
        .min(body_deadline);
    let mut input = [0; QUANTUM];
    let mut output = [0; QUANTUM];
    let mut used = request.prefix.len();
    charge(service, &mut meter, used as u64, 0)?;
    input
        .get_mut(..used)
        .ok_or(Error::Invalid)?
        .copy_from_slice(request.prefix);
    loop {
        sender.check()?;
        charge(service, &mut meter, 0, 0)?;
        let progress = reader
            .feed(input.get(..used).ok_or(Error::Invalid)?, &mut output)
            .map_err(body_error)?;
        if progress.written != 0 {
            charge(service, &mut meter, progress.written as u64, 0)?;
            if bytes
                .len()
                .checked_add(progress.written)
                .is_none_or(|n| n as u64 > maximum)
            {
                return Err(Error::Capacity);
            }
            bytes.extend_from_slice(output.get(..progress.written).ok_or(Error::Invalid)?);
            output
                .get_mut(..progress.written)
                .ok_or(Error::Invalid)?
                .fill(0);
        }
        if progress.consumed != used {
            return Err(Error::Invalid);
        }
        if progress.complete {
            break;
        }
        input.fill(0);
        used = match sender.transport.read(&mut input)? {
            IoProgress::Bytes(n) if n > 0 && n <= input.len() => {
                sender.check()?;
                charge(service, &mut meter, n as u64, 0)?;
                sender.idle =
                    timers::deadline(sender.last, service.timeouts.limits().body_idle_seconds)
                        .map_err(|_| Error::Invalid)?
                        .min(body_deadline);
                n
            }
            IoProgress::Pending => {
                std::thread::sleep(Duration::from_millis(5));
                0
            }
            _ => return Err(Error::Invalid),
        };
    }
    sender.check()?;
    let uploads = Uploads {
        loaded: service.loaded,
        executable: service.executable,
        binding: service.binding,
        slots: service.slots,
        clock: service.clock,
        pending: &service.pending.uploads,
        password: request.credentials.password(),
    };
    match uploads.create_admitted(
        Input {
            account: request.account,
            device: request.device,
            bytes: &bytes,
            deadline: request.deadline,
        },
        &mut meter,
        &ingress,
    ) {
        Ok(outcome) => Response::created(
            request.account,
            outcome.blob,
            outcome.length,
            request.content_type,
            false,
        ),
        Err(owner::Error::Publication(changes::Error::Indeterminate)) => Ok(Response::uncertain()),
        Err(error) => Err(publication_error(error)),
    }
}

fn charge(
    service: &Service<'_, '_, '_>,
    meter: &mut Meter,
    io_bytes: u64,
    output_bytes: u64,
) -> Result<(), Error> {
    meter
        .charge(
            service.clock.sample()?.monotonic,
            Charge {
                io_bytes,
                output_bytes,
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(|error| {
            if error == Stop::Deadline {
                Error::Deadline
            } else {
                Error::Capacity
            }
        })
}
fn body_error(error: crate::http_request::Error) -> Error {
    match error {
        crate::http_request::Error::Limit => Error::Capacity,
        _ => Error::Invalid,
    }
}
fn publication_error(error: owner::Error) -> Error {
    match error {
        owner::Error::Admission(error) => error,
        owner::Error::Publication(error) => match error {
            changes::Error::Unauthenticated | changes::Error::Policy => Error::Forbidden,
            changes::Error::Quota => Error::Quota,
            changes::Error::Capacity | changes::Error::Database | changes::Error::Credential(_) => {
                Error::Busy
            }
            changes::Error::Connection(postgres_probe::Error::Deadline) => Error::Deadline,
            changes::Error::Connection(postgres_probe::Error::Transport) => Error::Io {
                kind: std::io::ErrorKind::ConnectionReset,
                os_code: None,
            },
            changes::Error::Connection(postgres_probe::Error::Tls) => Error::Tls,
            _ => Error::Corrupt,
        },
    }
}
