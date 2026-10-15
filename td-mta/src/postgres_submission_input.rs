//! Freeze one verified Email and implicit envelope before queue publication.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    ids::{DeviceId, EmailId, IdentityId},
    mime_headers::{self, Scanner, Status},
    ports::{self, BlobReader, Clock, Deadline},
    postgres_body::Metadata,
    postgres_probe::{self, Request},
    postgres_read,
    postgres_runtime_profile::Binding,
    submission_envelope, transmitted_headers,
};
use std::{sync::Arc, time::Instant};
use td_crypto::TrustStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Read(postgres_read::Error),
    Work(ports::Error),
    Envelope(submission_envelope::Error),
    Headers,
    Identity,
    Unauthenticated,
    State,
    Capacity,
}

pub struct Input<'a> {
    pub binding: &'a Binding,
    pub email: EmailId,
    pub identity: IdentityId,
    pub expected: Option<u64>,
}

struct Data {
    binding: Binding,
    device: DeviceId,
    identity: IdentityId,
    expected: Option<u64>,
    source: Metadata,
    output: Output,
}
struct Output {
    bytes: Vec<u8>,
    reverse_path: String,
    recipients: Vec<String>,
}

/// Passive preparation only. Publication must recheck configuration, device,
/// source identity, state and quotas in its account transaction.
#[derive(Clone)]
pub struct Prepared(Arc<Data>);
impl Prepared {
    pub fn binding(&self) -> &Binding {
        &self.0.binding
    }
    pub fn device(&self) -> DeviceId {
        self.0.device
    }
    pub fn identity(&self) -> IdentityId {
        self.0.identity
    }
    pub fn expected(&self) -> Option<u64> {
        self.0.expected
    }
    pub fn source(&self) -> Metadata {
        self.0.source
    }
    pub fn bytes(&self) -> &[u8] {
        &self.0.output.bytes
    }
    pub fn reverse_path(&self) -> &str {
        &self.0.output.reverse_path
    }
    pub fn recipients(&self) -> impl ExactSizeIterator<Item = &str> {
        self.0.output.recipients.iter().map(String::as_str)
    }
}
impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedSubmission(<redacted>)")
    }
}

/// Read and verify under one authenticated snapshot, then release it. No
/// password, connection, quota reservation or delivery responsibility escapes.
pub fn prepare(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    input: Input<'_>,
    password: &[u8],
) -> Result<Prepared, Error> {
    let device = Presented::parse(password)
        .ok_or(Error::Unauthenticated)?
        .id();
    let identity = input
        .binding
        .identity_email(input.identity)
        .ok_or(Error::Identity)?;
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero() && *left <= postgres_probe::MAX_DURATION)
        .ok_or(Error::Work(ports::Error::Deadline))?;
    let clock = RuntimeClock::new();
    let now = clock.sample().map_err(Error::Work)?.monotonic;
    let cap = Deadline::after(
        now,
        u64::try_from(left.as_millis()).map_err(|_| Error::Capacity)?,
    )
    .map_err(Error::Work)?;
    let maximum = input.binding.upload_maximum().min(32 * 1024 * 1024);
    let work = input.binding.work();
    let mut meter = Meter::new(
        cap,
        Charge {
            io_bytes: work.foreground_io_bytes,
            records: work.foreground_records,
            output_bytes: maximum as u64,
            ..Charge::default()
        },
    );
    let result =
        postgres_read::with_snapshot(request, trust, deadline, input.binding, password, |view| {
            if input
                .expected
                .is_some_and(|expected| expected != view.identity().committed_sequence.number())
            {
                return Ok(Err(Error::State));
            }
            let metadata = view.email(input.email)?.ok_or(postgres_read::Error::Body(
                crate::postgres_body::Error::Missing,
            ))?;
            if metadata.body.length > maximum as u64 {
                return Ok(Err(Error::Capacity));
            }
            // Fund the reader's whole-body integrity pass before it starts.
            if let Err(error) = charge(
                &clock,
                &mut meter,
                Charge {
                    io_bytes: metadata.body.length,
                    records: metadata.body.length.div_ceil(65536),
                    ..Charge::default()
                },
            ) {
                return Ok(Err(error));
            }
            view.with_body(input.email, |source, reader| {
                Ok(freeze(
                    reader,
                    identity,
                    input.binding.header_maximum(),
                    input.binding.recipient_maximum(),
                    maximum,
                    &clock,
                    &mut meter,
                )
                .map(|output| {
                    Prepared(Arc::new(Data {
                        binding: input.binding.clone(),
                        device,
                        identity: input.identity,
                        expected: input.expected,
                        source,
                        output,
                    }))
                }))
            })
        })
        .map_err(Error::Read)??;
    if Instant::now() >= deadline {
        return Err(Error::Work(ports::Error::Deadline));
    }
    Ok(result)
}

fn stop(error: Stop) -> ports::Error {
    if error == Stop::Deadline {
        ports::Error::Deadline
    } else {
        ports::Error::Capacity
    }
}
fn charge(clock: &dyn Clock, meter: &mut Meter, value: Charge) -> Result<(), Error> {
    let now = clock.sample().map_err(Error::Work)?.monotonic;
    meter
        .charge(now, value)
        .map_err(|error| Error::Work(stop(error)))
}
fn copy(value: &str) -> Result<String, Error> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| Error::Capacity)?;
    owned.push_str(value);
    Ok(owned)
}

fn freeze(
    reader: &mut dyn BlobReader,
    identity: &str,
    header_maximum: usize,
    recipient_maximum: usize,
    maximum: usize,
    clock: &dyn Clock,
    meter: &mut Meter,
) -> Result<Output, Error> {
    if header_maximum > crate::limits::MIB
        || header_maximum == 0
        || reader.len() > maximum as u64
        || maximum > 32 * 1024 * 1024
    {
        return Err(Error::Capacity);
    }
    let ceiling = reader.len().min(header_maximum as u64 + 2) as usize;
    let mut headers = Vec::new();
    headers
        .try_reserve_exact(ceiling)
        .map_err(|_| Error::Capacity)?;
    headers.resize(ceiling, 0);
    let mut scanner = Scanner::new(0, header_maximum as u64);
    let mut filled = 0;
    let mut consumed = 0;
    let mut needs_input = true;
    let end = loop {
        charge(clock, meter, Charge::default())?;
        if needs_input {
            let count = ceiling.saturating_sub(filled).min(4096);
            if count == 0 {
                return Err(Error::Headers);
            }
            charge(
                clock,
                meter,
                Charge {
                    io_bytes: count as u64,
                    records: 1,
                    ..Charge::default()
                },
            )?;
            let target = headers
                .get_mut(filled..filled + count)
                .ok_or(Error::Headers)?;
            let read = reader.read_at(filled as u64, target).map_err(Error::Work)?;
            if read == 0 || read > count {
                return Err(Error::Work(ports::Error::Corrupt));
            }
            filled += read;
            needs_input = false;
        }
        let now = clock.sample().map_err(Error::Work)?.monotonic;
        let available = headers.get(consumed..filled).ok_or(Error::Headers)?;
        let progress = scanner
            .poll(available, filled as u64 == reader.len(), now, meter)
            .map_err(|error| match error {
                mime_headers::Error::Work(error) => Error::Work(stop(error)),
                _ => Error::Headers,
            })?;
        if progress.consumed > available.len() {
            return Err(Error::Headers);
        }
        consumed += progress.consumed;
        match progress.status {
            Status::Complete(end) => break end,
            Status::NeedInput => {
                if consumed != filled || filled as u64 == reader.len() {
                    return Err(Error::Headers);
                }
                needs_input = true;
            }
            Status::Field(_) | Status::Yield => (),
        }
    };
    let body_start = usize::try_from(end.body_start).map_err(|_| Error::Headers)?;
    let headers = headers.get(..body_start).ok_or(Error::Headers)?;
    let deadline =
        Deadline::after(ports::Tick(0), meter.deadline().tick().0).map_err(Error::Work)?;
    let envelope =
        submission_envelope::derive(headers, identity, recipient_maximum, deadline, clock, meter)
            .map_err(Error::Envelope)?;
    let reverse_path = copy(envelope.mail_from())?;
    let mut recipients = Vec::new();
    recipients
        .try_reserve_exact(envelope.recipients().len())
        .map_err(|_| Error::Capacity)?;
    for address in envelope.recipients() {
        charge(
            clock,
            meter,
            Charge {
                io_bytes: address.len() as u64,
                records: 1,
                ..Charge::default()
            },
        )?;
        recipients.push(copy(address)?);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(reader.len() as usize)
        .map_err(|_| Error::Capacity)?;
    transmitted_headers::write(headers, clock, deadline, meter, |chunk| {
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > maximum)
        {
            return Err(ports::Error::Capacity);
        }
        bytes.extend_from_slice(chunk);
        Ok(())
    })
    .map_err(Error::Work)?;
    let mut offset = end.body_start;
    let mut scratch = [0; 4096];
    while offset < reader.len() {
        let count = (reader.len() - offset).min(scratch.len() as u64) as usize;
        charge(
            clock,
            meter,
            Charge {
                io_bytes: count as u64,
                output_bytes: count as u64,
                records: 1,
                ..Charge::default()
            },
        )?;
        let read = reader
            .read_at(offset, scratch.get_mut(..count).ok_or(Error::Headers)?)
            .map_err(Error::Work)?;
        if read == 0
            || read > count
            || bytes
                .len()
                .checked_add(read)
                .is_none_or(|len| len > maximum)
        {
            return Err(Error::Work(ports::Error::Corrupt));
        }
        bytes.extend_from_slice(scratch.get(..read).ok_or(Error::Headers)?);
        offset += read as u64;
    }
    charge(clock, meter, Charge::default())?;
    Ok(Output {
        bytes,
        reverse_path,
        recipients,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::ports::{Tick, Time};
    struct Now;
    impl Clock for Now {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    struct Source<'a> {
        bytes: &'a [u8],
        step: usize,
        fail_at: Option<u64>,
    }
    impl BlobReader for Source<'_> {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, ports::Error> {
            if self.fail_at.is_some_and(|position| offset >= position) {
                return Err(ports::Error::Corrupt);
            }
            let source = self
                .bytes
                .get(offset as usize..)
                .ok_or(ports::Error::Invalid)?;
            let count = source.len().min(out.len()).min(self.step);
            out[..count].copy_from_slice(&source[..count]);
            Ok(count)
        }
    }
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 16_000_000,
                records: 100_000,
                output_bytes: 16_000_000,
                ..Charge::default()
            },
        )
    }
    fn run(bytes: &[u8], step: usize) -> Result<Output, Error> {
        freeze(
            &mut Source {
                bytes,
                step,
                fail_at: None,
            },
            "main@example.test",
            262144,
            100,
            1024 * 1024,
            &Now,
            &mut meter(),
        )
    }
    #[test]
    fn fragmented_reads_preserve_body_and_strip_blind_fields() {
        let headers = b"From: main@example.test\r\nTo: one@example.test\r\nBcc: hidden@example.test,\r\n one@EXAMPLE.test\r\nResent-Bcc: other@example.test\r\nX-Keep: yes\r\n\r\n";
        let body = b"Bcc: body bytes stay\r\n\0\xff";
        let input = [headers.as_slice(), body].concat();
        for step in [1, 2, 7, 4096] {
            let output = run(&input, step).unwrap();
            assert_eq!(output.reverse_path, "main@example.test");
            assert_eq!(
                output.recipients,
                ["one@example.test", "hidden@example.test"]
            );
            assert_eq!(
                output.bytes,
                [
                    b"From: main@example.test\r\nTo: one@example.test\r\nX-Keep: yes\r\n\r\n"
                        .as_slice(),
                    body
                ]
                .concat()
            );
        }
        let mut large = input[..input.len() - body.len()].to_vec();
        large.extend(std::iter::repeat_n(b'x', 12000));
        assert!(run(&large, 127)
            .unwrap()
            .bytes
            .ends_with(&vec![b'x'; 12000]));
    }
    #[test]
    fn identity_headers_work_size_and_source_failures_refuse() {
        let input = b"From: main@example.test\r\nTo: one@example.test\r\n\r\nbody";
        assert!(matches!(
            run(
                b"From: other@example.test\r\nTo: one@example.test\r\n\r\n",
                4096
            ),
            Err(Error::Envelope(submission_envelope::Error::ForbiddenFrom))
        ));
        for bytes in [
            b"From: main@example.test".as_slice(),
            b"From: main@example.test\nTo: one@example.test\n\nbody",
        ] {
            assert!(run(bytes, 4096).is_err());
        }
        let source = || Source {
            bytes: input,
            step: 4096,
            fail_at: None,
        };
        assert!(freeze(
            &mut source(),
            "main@example.test",
            10,
            100,
            1024,
            &Now,
            &mut meter()
        )
        .is_err());
        assert!(matches!(
            freeze(
                &mut source(),
                "main@example.test",
                100,
                100,
                10,
                &Now,
                &mut meter()
            ),
            Err(Error::Capacity)
        ));
        let mut exhausted = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
        assert!(matches!(
            freeze(
                &mut source(),
                "main@example.test",
                100,
                100,
                1024,
                &Now,
                &mut exhausted
            ),
            Err(Error::Work(ports::Error::Capacity))
        ));
        let mut expired = Meter::new(Deadline::after(Tick(0), 1).unwrap(), Charge::default());
        assert!(matches!(
            freeze(
                &mut source(),
                "main@example.test",
                100,
                100,
                1024,
                &Now,
                &mut expired
            ),
            Err(Error::Work(ports::Error::Deadline))
        ));
        let mut failed = Source {
            fail_at: Some(0),
            ..source()
        };
        assert!(matches!(
            freeze(
                &mut failed,
                "main@example.test",
                100,
                100,
                1024,
                &Now,
                &mut meter()
            ),
            Err(Error::Work(ports::Error::Corrupt))
        ));
    }
}
