//! Authenticated immutable downloads on the fixed HTTPS body worker.
use crate::{
    admission::timers::{self, TimeoutPlan},
    bounded::TextBuffer,
    http_request::{decode_uri_component, QUANTUM},
    https_exchange::Sender,
    ids::{AccountId, BlobId, DeviceId},
    ports::{BlobReader, Clock, Deadline, Error, Transport},
    smtp_receiving::Control,
    store_fs::{BlobAuthorization, BlobReadRequest, StoreCoordinator},
    wire,
};

pub struct Target {
    account: AccountId,
    blob: BlobId,
    part: Option<wire::PartLocator>,
    name: [u8; 4096],
    name_len: usize,
    media: [u8; 1024],
    media_len: usize,
}
impl Target {
    pub fn parse(target: &str) -> Result<Self, Error> {
        let path = target
            .strip_prefix("/jmap/download/")
            .ok_or(Error::NotFound)?;
        let (account, path) = path.split_once('/').ok_or(Error::Invalid)?;
        let (blob, path) = path.split_once('/').ok_or(Error::Invalid)?;
        let (name, query) = path.split_once('?').ok_or(Error::Invalid)?;
        if name.contains('/') || query.contains('&') {
            return Err(Error::Invalid);
        }
        let media = query.strip_prefix("type=").ok_or(Error::Invalid)?;
        let mut id = [0; 255];
        let account = wire::Id::parse(decode_uri_component(account, &mut id)?)
            .map_err(|_| Error::Invalid)?
            .local()
            .ok_or(Error::NotFound)?;
        let reference = wire::Id::parse(decode_uri_component(blob, &mut id)?)
            .map_err(|_| Error::Invalid)?
            .blob()
            .ok_or(Error::NotFound)?;
        let (blob, part) = match reference {
            wire::BlobReference::File(blob) => (blob, None),
            wire::BlobReference::Part(part) => (part.parent, Some(part)),
            wire::BlobReference::Nested(_) => return Err(Error::NotFound),
        };
        let mut out = Self {
            account,
            blob,
            part,
            name: [0; 4096],
            name_len: 0,
            media: [0; 1024],
            media_len: 0,
        };
        let name = decode_uri_component(name, &mut out.name)?;
        if name.chars().any(char::is_control) {
            return Err(Error::Invalid);
        }
        out.name_len = name.len();
        let media = decode_uri_component(media, &mut out.media)?;
        if media.is_empty() || !media.bytes().all(|b| (32..=126).contains(&b)) {
            return Err(Error::Invalid);
        }
        out.media_len = media.len();
        Ok(out)
    }
    pub fn account(&self) -> AccountId {
        self.account
    }
}
pub struct Request<'a> {
    pub target: &'a Target,
    pub device: DeviceId,
    pub deadline: Deadline,
}
#[derive(Debug)]
pub enum Failure {
    Before(Error),
    Started(Error),
}

pub(crate) struct Settings<'a> {
    pub maximum: u64,
    pub limits: &'a crate::limits::Limits,
}
#[path = "https_download_part.rs"]
mod part;

pub(crate) fn send<P: BlobAuthorization>(
    coordinator: &StoreCoordinator<'_, '_, P>,
    settings: Settings<'_>,
    request: Request<'_>,
    transport: &mut dyn Transport,
    control: &Control,
    clock: &dyn Clock,
    timeouts: &TimeoutPlan,
) -> Result<(), Failure> {
    let mut sender = Sender::new(transport, control, clock, timeouts, request.deadline)
        .map_err(Failure::Before)?;
    let started_at = sender.last;
    let target = request.target;
    let result = coordinator.with_authorized_blob_pin(
        &td_crypto::Provider,
        BlobReadRequest {
            account: target.account,
            device: request.device,
            id: target.blob,
            maximum: settings.maximum,
            deadline: request.deadline,
        },
        |body, work| {
            sender.check()?;
            sender.deadline = timers::deadline(
                started_at,
                timeouts
                    .http_exchange_seconds(0, body.len())
                    .map_err(|_| Error::Invalid)?,
            )
            .map_err(|_| Error::Invalid)?
            .min(request.deadline);
            let decoded = match target.part {
                Some(locator) => Some(part::prepare(
                    body,
                    locator,
                    settings.limits,
                    work,
                    clock,
                    &mut sender,
                )?),
                None => None,
            };
            let length = decoded.as_ref().map_or(body.len(), |part| part.length);
            sender.check()?;
            sender.idle = timers::deadline(sender.last, timeouts.limits().response_idle_seconds)
                .map_err(|_| Error::Invalid)?
                .min(sender.deadline);
            let mut buffer = [0; QUANTUM];
            let mut header = TextBuffer::new(&mut buffer);
            let media =
                std::str::from_utf8(target.media.get(..target.media_len).ok_or(Error::Invalid)?)
                    .map_err(|_| Error::Invalid)?;
            header
                .format(format_args!(
                    concat!(
                        "HTTP/1.1 200 OK\r\nDate: {}\r\nConnection: close\r\n",
                        "Content-Length: {}\r\nContent-Type: {}\r\n",
                        "Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n",
                        "Content-Disposition: attachment; filename*=UTF-8''"
                    ),
                    crate::https::http_date(clock.sample()?.utc_ms)?,
                    length,
                    media
                ))
                .map_err(|_| Error::Capacity)?;
            for byte in target.name.get(..target.name_len).ok_or(Error::Invalid)? {
                header
                    .format(format_args!("%{byte:02X}"))
                    .map_err(|_| Error::Capacity)?;
            }
            header.append("\r\n\r\n").map_err(|_| Error::Capacity)?;
            let length = header.len();
            sender.write(buffer.get(..length).ok_or(Error::Invalid)?)?;
            if let Some(part) = decoded {
                part::send(body, part, work, clock, &mut sender, &mut buffer)?;
                return sender.flush();
            }
            let mut offset = 0u64;
            while offset < body.len() {
                sender.check()?;
                let count = body.read_at(offset, &mut buffer)?;
                if count == 0 || count > buffer.len() || count as u64 > body.len() - offset {
                    return Err(Error::Corrupt);
                }
                sender.write(buffer.get(..count).ok_or(Error::Invalid)?)?;
                offset = offset.checked_add(count as u64).ok_or(Error::Corrupt)?;
                buffer.get_mut(..count).ok_or(Error::Invalid)?.fill(0);
            }
            sender.flush()
        },
    );
    result.map_err(|error| {
        if sender.started {
            Failure::Started(error)
        } else {
            Failure::Before(error)
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::ports::{FlushProgress, IoProgress, Tick};
    use std::sync::atomic::{AtomicU64, Ordering};
    fn url(name: &str, media: &str) -> String {
        format!(
            "/jmap/download/a{}/b{}/{name}?type={media}",
            "11".repeat(16),
            "22".repeat(16)
        )
    }
    #[test]
    fn download_target_decodes_once_and_refuses_header_injection() {
        let target = Target::parse(&url(
            "snow%E2%98%83%252F+",
            "text%2Fplain%3B%20charset%3Dutf-8",
        ))
        .unwrap();
        assert_eq!(
            target.name.get(..target.name_len),
            Some("snow☃%2F+".as_bytes())
        );
        assert_eq!(
            target.media.get(..target.media_len),
            Some(b"text/plain; charset=utf-8".as_slice())
        );
        for (name, media) in [
            ("%00", "text/plain"),
            ("%0D%0A", "text/plain"),
            ("%FF", "text/plain"),
            ("a", "text%2Fplain%0D%0AX-Test%3Ayes"),
            ("a", ""),
            ("a/b", "text/plain"),
            ("a", "text/plain&x=y"),
            ("%", "text/plain"),
        ] {
            assert!(matches!(
                Target::parse(&url(name, media)),
                Err(Error::Invalid)
            ));
        }
        assert!(matches!(
            Target::parse(&url("a", &"x".repeat(1025))),
            Err(Error::Capacity)
        ));
        assert!(matches!(
            Target::parse("/jmap/download/foreign/absent/a?type=text/plain"),
            Err(Error::NotFound)
        ));
        assert!(matches!(
            Target::parse(&url(&"n".repeat(4097), "text/plain")),
            Err(Error::Capacity)
        ));
        assert!(Target::parse(&url(&"n".repeat(4096), "text/plain")).is_ok());
        let long_id = format!(
            "/jmap/download/{}/b{}/a?type=text/plain",
            "a".repeat(256),
            "22".repeat(16)
        );
        assert!(matches!(Target::parse(&long_id), Err(Error::Capacity)));
        assert!(Target::parse(&url("", "application%2Foctet-stream")).is_ok());
    }
    struct ClockNow(AtomicU64);
    impl Clock for ClockNow {
        fn sample(&self) -> Result<crate::ports::Time, Error> {
            Ok(crate::ports::Time {
                utc_ms: 0,
                monotonic: Tick(self.0.load(Ordering::Relaxed)),
            })
        }
    }
    struct Wire {
        bytes: Vec<u8>,
        pending: bool,
        fail: bool,
    }
    impl Transport for Wire {
        fn read(&mut self, _: &mut [u8]) -> Result<IoProgress, Error> {
            Err(Error::Invalid)
        }
        fn write(&mut self, bytes: &[u8]) -> Result<IoProgress, Error> {
            if self.fail {
                return Err(Error::Tls);
            }
            if self.pending {
                self.pending = false;
                return Ok(IoProgress::Pending);
            }
            self.bytes
                .extend_from_slice(bytes.get(..1).ok_or(Error::Invalid)?);
            Ok(IoProgress::Bytes(1))
        }
        fn flush(&mut self) -> Result<FlushProgress, Error> {
            Ok(FlushProgress::Complete)
        }
        fn close(&mut self) -> Result<FlushProgress, Error> {
            Ok(FlushProgress::Complete)
        }
        fn abort(&mut self) {}
    }
    #[test]
    fn sender_handles_partial_writes_and_original_deadline_without_restarting_response() {
        use crate::admission::{timers::NetworkLimits, DiskLimits, ViewMode, WorkLimits};
        let resources = crate::limits::Limits::default().plan().unwrap();
        let admission = DiskLimits::default()
            .plan(
                &resources,
                WorkLimits::default(),
                ViewMode::OnlineBackground,
            )
            .unwrap();
        let timeouts = NetworkLimits::default().plan(&admission).unwrap();
        let clock = ClockNow(AtomicU64::new(1));
        let control = Control::default();
        let mut wire = Wire {
            bytes: Vec::new(),
            pending: true,
            fail: false,
        };
        let until = Deadline::after(Tick(1), 100).unwrap();
        let mut sender = Sender::new(&mut wire, &control, &clock, &timeouts, until).unwrap();
        sender.write(b"abc").unwrap();
        sender.flush().unwrap();
        assert!(sender.started);
        clock.0.store(101, Ordering::Relaxed);
        assert_eq!(sender.write(b"never"), Err(Error::Deadline));
        assert_eq!(wire.bytes, b"abc");
        clock.0.store(1, Ordering::Relaxed);
        wire.fail = true;
        let mut sender = Sender::new(&mut wire, &control, &clock, &timeouts, until).unwrap();
        assert_eq!(sender.write(b"header"), Err(Error::Tls));
        assert!(sender.started);
        sender.idle = Deadline::after(Tick(1), 1).unwrap();
        clock.0.store(2, Ordering::Relaxed);
        assert_eq!(sender.flush(), Err(Error::Deadline));
        clock.0.store(3, Ordering::Relaxed);
        control.stop();
        assert_eq!(sender.flush(), Err(Error::Deadline));
    }
}
