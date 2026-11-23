//! Independent SMTP and HTTPS listener samples before and during the writer fence.
use crate::{
    common::{durable, fmt, owned, require, set},
    R,
};
use std::{
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use td_crypto::{ClientConfig, ClockHandle, TlsProtocol, TlsSession, TrustStore};
use td_json::Json;
use td_mta::{
    clock::{RuntimeClock, TlsClockSource},
    ports::{Clock, Deadline, FlushProgress, IoProgress, Transport},
    tls_io::{TlsIo, TLS_WIRE_BYTES},
    transport::TcpTransport,
};

const PROBES: &[(&str, u16)] = &[("smtp", 25), ("jmap", 443)];
type Data = Vec<(String, Vec<(f64, bool)>)>;
struct Records {
    stop: AtomicBool,
    data: Mutex<Data>,
}
pub struct Probes {
    /// The public name, from host.json, the listeners' certificate presents.
    name: String,
    records: td_alloc::BudgetShared<Records>,
    threads: Vec<JoinHandle<()>>,
}
pub fn now() -> R<f64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64())
}

fn pause(end: Instant) -> R<()> {
    require(Instant::now() < end, "listener probe timeout")?;
    thread::sleep(Duration::from_millis(1));
    Ok(())
}
fn write(transport: &mut impl Transport, mut data: &[u8], end: Instant) -> R<()> {
    while !data.is_empty() {
        require(Instant::now() < end, "listener probe timeout")?;
        match transport.write(data)? {
            IoProgress::Bytes(n) if n != 0 => data = data.get(n..).ok_or("short probe write")?,
            IoProgress::Pending => pause(end)?,
            _ => return Err("closed probe stream".into()),
        }
    }
    while transport.flush()? == FlushProgress::Pending {
        pause(end)?;
    }
    Ok(())
}
fn line(transport: &mut impl Transport, end: Instant, buf: &mut [u8; 1024]) -> R<usize> {
    for (i, slot) in buf.iter_mut().enumerate() {
        require(Instant::now() < end, "listener probe timeout")?;
        loop {
            match transport.read(std::slice::from_mut(slot))? {
                IoProgress::Bytes(1) => break,
                IoProgress::Pending => pause(end)?,
                _ => return Err("short listener response".into()),
            }
        }
        if *slot == b'\n' {
            return Ok(i + 1);
        }
    }
    Err("listener line too long".into())
}
fn smtp(transport: &mut impl Transport, code: &[u8; 3], end: Instant) -> R<bool> {
    let mut starttls = false;
    for _ in 0..64 {
        let mut buf = [0; 1024];
        let n = line(transport, end, &mut buf)?;
        let msg = buf.get(..n).ok_or("SMTP length")?;
        require(
            msg.get(..3) == Some(code) && msg.ends_with(b"\r\n"),
            "unexpected SMTP response",
        )?;
        starttls |= msg
            .get(4..)
            .is_some_and(|v| v.eq_ignore_ascii_case(b"STARTTLS\r\n"));
        match msg.get(3) {
            Some(b' ') => return Ok(starttls),
            Some(b'-') => (),
            _ => return Err("SMTP framing".into()),
        }
    }
    Err("too many SMTP lines".into())
}
struct Listener {
    protocol: TlsProtocol,
    address: SocketAddr,
    clock: Arc<dyn Clock>,
    config: Arc<ClientConfig>,
    name: String,
    request: String,
}
impl Listener {
    fn production(name: &str, public: &str) -> R<Self> {
        let &(_, port) = PROBES
            .iter()
            .find(|(n, _)| *n == name)
            .ok_or("unknown listener probe")?;
        let protocol = if name == "smtp" {
            TlsProtocol::Smtp
        } else {
            TlsProtocol::Http1
        };
        Self::new(
            protocol,
            (Ipv4Addr::LOCALHOST, port).into(),
            &TrustStore::public_roots()?,
            Arc::new(RuntimeClock::new()),
            public,
        )
    }

    fn new(
        protocol: TlsProtocol,
        address: SocketAddr,
        trust: &TrustStore,
        clock: Arc<dyn Clock>,
        name: &str,
    ) -> R<Self> {
        let config = ClientConfig::new(
            trust,
            Arc::new(ClockHandle::new(TlsClockSource::new(Arc::clone(&clock)))),
            protocol,
        )?;
        Ok(Self {
            protocol,
            address,
            clock,
            config: Arc::new(config),
            name: owned(name)?,
            request: fmt(format_args!(
                "GET /.well-known/jmap HTTP/1.1\r\nHost: {name}\r\nConnection: close\r\n\r\n"
            ))?,
        })
    }

    fn probe(&self, timeout: Duration) -> R<()> {
        let end = Instant::now() + timeout;
        let deadline = Deadline::after(self.clock.monotonic()?, timeout.as_millis().try_into()?)?;
        let mut tcp =
            TcpTransport::from_stream(TcpStream::connect_timeout(&self.address, timeout)?)?;
        if self.protocol == TlsProtocol::Smtp {
            smtp(&mut tcp, b"220", end)?;
            write(&mut tcp, b"EHLO probe.invalid\r\n", end)?;
            require(smtp(&mut tcp, b"250", end)?, "SMTP STARTTLS unavailable")?;
            write(&mut tcp, b"STARTTLS\r\n", end)?;
            smtp(&mut tcp, b"220", end)?;
        }
        let session = TlsSession::client(Arc::clone(&self.config), &self.name)?;
        let (mut input, mut output) = ([0u8; TLS_WIRE_BYTES], [0u8; TLS_WIRE_BYTES]);
        let mut tls = TlsIo::new(
            tcp,
            session,
            Arc::clone(&self.clock),
            deadline,
            deadline,
            &mut input,
            &mut output,
        )
        .map_err(|_| "TLS probe setup")?;
        while tls.handshake()?.is_none() {
            pause(end)?;
        }
        if self.protocol == TlsProtocol::Smtp {
            write(&mut tls, b"EHLO probe.invalid\r\n", end)?;
            smtp(&mut tls, b"250", end)?;
            write(&mut tls, b"QUIT\r\n", end)?;
            smtp(&mut tls, b"221", end)?;
        } else {
            write(&mut tls, self.request.as_bytes(), end)?;
            let mut buf = [0; 1024];
            let n = line(&mut tls, end, &mut buf)?;
            let header = std::str::from_utf8(buf.get(..n).ok_or("HTTP length")?)?;
            let mut fields = header.split_ascii_whitespace();
            require(
                header.ends_with("\r\n")
                    && matches!(fields.next(), Some("HTTP/1.0" | "HTTP/1.1"))
                    && fields.next() == Some("401"),
                "JMAP authentication challenge",
            )?;
        }
        Ok(())
    }
}

impl Probes {
    pub fn new(name: &str) -> R<Self> {
        // One shared control block plus its fixed-size record-list header.
        let budget = td_alloc::Budget::new(1024)?;
        let records = td_alloc::BudgetShared::new(
            &budget,
            Records {
                stop: AtomicBool::new(false),
                data: Mutex::new(Vec::new()),
            },
        )?;
        Ok(Self {
            name: owned(name)?,
            records,
            threads: Vec::new(),
        })
    }
    pub fn probe(&self, name: &str) -> R<()> {
        Listener::production(name, &self.name)?.probe(Duration::from_secs(1))
    }
    pub fn start(&mut self, require_ready: bool) -> R<()> {
        td_alloc::reserve(&mut self.threads, PROBES.len())?;
        for &(name, _) in PROBES {
            let listener = Listener::production(name, &self.name)?;
            if require_ready {
                listener.probe(Duration::from_secs(1))?;
            }
            td_alloc::push(
                &mut *self.records.data.lock().map_err(|_| "probe records")?,
                (owned(name)?, Vec::new()),
            )?;
            let records = self.records.try_clone()?;
            let handle = thread::Builder::new().spawn(move || {
                while !records.stop.load(Ordering::Acquire) {
                    let start = Instant::now();
                    let ok = listener.probe(Duration::from_secs(1)).is_ok();
                    let Ok(mut rows) = records.data.lock() else {
                        break;
                    };
                    let Some((_, samples)) = rows.iter_mut().find(|(key, _)| key == name) else {
                        break;
                    };
                    let Ok(stamp) = now() else {
                        break;
                    };
                    if samples.len() >= 32768 || td_alloc::push(samples, (stamp, ok)).is_err() {
                        break;
                    }
                    drop(rows);
                    thread::park_timeout(
                        Duration::from_millis(500).saturating_sub(start.elapsed()),
                    );
                }
            })?;
            td_alloc::push(&mut self.threads, handle)?;
        }
        Ok(())
    }
    pub fn samples(&self) -> R<Data> {
        let mut result = Vec::new();
        for (name, samples) in self
            .records
            .data
            .lock()
            .map_err(|_| "probe records")?
            .iter()
        {
            td_alloc::push(&mut result, (owned(name)?, td_alloc::copy(samples)?))?;
        }
        Ok(result)
    }
    fn stop(&mut self) {
        self.records.stop.store(true, Ordering::Release);
        for handle in self.threads.drain(..) {
            handle.thread().unpark();
            let _ = JoinHandle::join(handle);
        }
    }
    pub fn finish(&mut self, path: &Path) -> R<()> {
        self.stop();
        let mut obj = Json::Obj(Vec::new());
        for (name, samples) in self.samples()? {
            let mut rows = Vec::new();
            for (ts, ok) in samples {
                let mut pair = Vec::new();
                td_alloc::push(&mut pair, Json::from(ts))?;
                td_alloc::push(&mut pair, Json::Bool(ok))?;
                td_alloc::push(&mut rows, Json::Arr(pair))?;
            }
            set(&mut obj, &name, Json::Arr(rows))?;
        }
        durable(path, &obj)
    }
}
impl Drop for Probes {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
#[path = "tests/probes.rs"]
mod tests;
