//! The byte stream under td-postgres: the input filter and output windows
//! around a private Unix socket or, transitionally, a verified TLS stream.
use super::framing::Input;
use crate::{
    ports::{Clock, Deadline, FlushProgress, IoProgress, Transport},
    tls_io::{TlsIo, TLS_WIRE_BYTES},
    transport::TcpTransport,
};
use std::{
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use td_crypto::TlsSession;
use td_postgres::{Socket, Stream, StreamError, TlsConnect};

/// TLS hands the client at most this much plaintext a read.
const READ_BYTES: usize = 16 * 1024;

enum Inner<S> {
    Plain(S),
    Tls(Box<Secure>),
}

/// Every byte td-postgres reads passes `Input` before the client parses
/// it, and every request is charged to the connection's output allowance
/// and the open window before it is written. A refusal, or the worker's
/// abort, fails the stream for good.
pub(crate) struct Wire<S = Socket> {
    inner: Inner<S>,
    input: Input,
    output_left: usize,
    aborted: Arc<AtomicBool>,
}

impl<S: Stream> Wire<S> {
    pub(super) fn plain(stream: S, input: Input, output: usize, abort: Arc<AtomicBool>) -> Self {
        Self {
            inner: Inner::Plain(stream),
            input,
            output_left: output,
            aborted: abort,
        }
    }

    fn refuse(&mut self) -> StreamError {
        self.input.fail();
        self.aborted.store(true, Ordering::Release);
        if let Inner::Tls(secure) = &mut self.inner {
            secure.0.abort();
        }
        StreamError::Failed
    }

    fn live(&self) -> Result<(), StreamError> {
        if self.aborted.load(Ordering::Acquire) {
            Err(StreamError::Failed)
        } else {
            Ok(())
        }
    }
}

impl<S: Stream> Stream for Wire<S> {
    fn read(&mut self, buffer: &mut [u8], deadline: Instant) -> Result<usize, StreamError> {
        self.live()?;
        let count = match &mut self.inner {
            Inner::Plain(stream) => stream.read(buffer, deadline)?,
            Inner::Tls(secure) => secure.read(buffer, deadline)?,
        };
        let received = buffer.get(..count).ok_or(StreamError::Failed)?;
        if self.input.accept(received).is_err() {
            return Err(self.refuse());
        }
        Ok(count)
    }

    fn write_all(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), StreamError> {
        self.live()?;
        if bytes.len() > self.output_left || self.input.admit_output(bytes.len()).is_err() {
            return Err(self.refuse());
        }
        match &mut self.inner {
            Inner::Plain(stream) => stream.write_all(bytes, deadline)?,
            Inner::Tls(secure) => secure.write_all(bytes, deadline)?,
        }
        self.output_left -= bytes.len();
        if self.input.output(bytes.len()).is_err() {
            return Err(self.refuse());
        }
        Ok(())
    }
}

impl<S> Drop for Wire<S> {
    /// After Terminate, a live TLS stream sends close_notify if its socket
    /// takes it at once; an aborted one just ends.
    fn drop(&mut self) {
        if let Inner::Tls(secure) = &mut self.inner {
            if !self.aborted.load(Ordering::Acquire) {
                let _ = secure.0.close();
            }
        }
    }
}

type Tls = TlsIo<TcpTransport, Box<[u8; TLS_WIRE_BYTES]>>;

/// A blocking view of the nonblocking TLS adapter, polling its socket.
/// Transitional: it serves only the TCP endpoint.
struct Secure(Tls);

fn stream_error(error: crate::ports::Error) -> StreamError {
    if error == crate::ports::Error::Deadline {
        StreamError::TimedOut
    } else {
        StreamError::Failed
    }
}

/// Wait a moment for progress the socket cannot signal, or time out.
fn pause(deadline: Instant) -> Result<(), StreamError> {
    if Instant::now() >= deadline {
        return Err(StreamError::TimedOut);
    }
    std::thread::sleep(Duration::from_millis(1));
    Ok(())
}

impl Secure {
    fn read(&mut self, buffer: &mut [u8], deadline: Instant) -> Result<usize, StreamError> {
        let length = buffer.len().min(READ_BYTES);
        let buffer = buffer.get_mut(..length).ok_or(StreamError::Failed)?;
        loop {
            match self.0.read(buffer).map_err(stream_error)? {
                IoProgress::Bytes(count) => return Ok(count),
                IoProgress::Closed => return Ok(0),
                IoProgress::Pending => {
                    if Instant::now() >= deadline {
                        return Err(StreamError::TimedOut);
                    }
                    if !self.0.wait_input(deadline).map_err(stream_error)? {
                        pause(deadline)?;
                    }
                }
            }
        }
    }

    fn write_all(&mut self, mut bytes: &[u8], deadline: Instant) -> Result<(), StreamError> {
        if Instant::now() >= deadline {
            return Err(StreamError::TimedOut);
        }
        while !bytes.is_empty() {
            match self.0.write(bytes).map_err(stream_error)? {
                IoProgress::Bytes(count) => {
                    bytes = bytes.get(count..).ok_or(StreamError::Failed)?;
                }
                IoProgress::Pending => pause(deadline)?,
                IoProgress::Closed => return Err(StreamError::Failed),
            }
        }
        while self.0.flush().map_err(stream_error)? != FlushProgress::Complete {
            pause(deadline)?;
        }
        Ok(())
    }
}

/// The TLS connector td-postgres calls once the server accepts the
/// SSLRequest: td-crypto's verified session over the bare socket, then
/// the same filter and windows as a Unix connection. Transitional, with
/// the TCP endpoint it serves.
pub(super) struct Connector {
    pub(super) session: TlsSession,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) cap: Deadline,
    pub(super) handshake_cap: Deadline,
    pub(super) output: usize,
    pub(super) input: Input,
    pub(super) abort: Arc<AtomicBool>,
}

impl TlsConnect for Connector {
    type Stream = Wire;

    fn connect(self, socket: TcpStream, deadline: Instant) -> Result<Wire, StreamError> {
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbTls);
        let transport = TcpTransport::from_stream(socket).map_err(|_| StreamError::Failed)?;
        let mut secure = TlsIo::new(
            transport,
            self.session,
            self.clock,
            self.cap,
            self.handshake_cap,
            Box::new([0; TLS_WIRE_BYTES]),
            Box::new([0; TLS_WIRE_BYTES]),
        )
        .map_err(|refusal| stream_error(refusal.error()))?;
        while secure.handshake().map_err(stream_error)?.is_none() {
            if !secure
                .wait_handshake_input(deadline)
                .map_err(stream_error)?
            {
                pause(deadline)?;
            }
        }
        timing.succeeded();
        Ok(Wire {
            inner: Inner::Tls(Box::new(Secure(secure))),
            input: self.input,
            output_left: self.output,
            aborted: self.abort,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    /// A blocking socket for the filter to wrap.
    struct Pair(UnixStream);

    impl Stream for Pair {
        fn read(&mut self, buffer: &mut [u8], _: Instant) -> Result<usize, StreamError> {
            self.0.read(buffer).map_err(|_| StreamError::Failed)
        }
        fn write_all(&mut self, bytes: &[u8], _: Instant) -> Result<(), StreamError> {
            Write::write_all(&mut self.0, bytes).map_err(|_| StreamError::Failed)
        }
    }

    fn pair(input: Input, output: usize) -> (Wire<Pair>, UnixStream, Arc<AtomicBool>) {
        let (near, far) = UnixStream::pair().unwrap();
        let abort = Arc::new(AtomicBool::new(false));
        (
            Wire::plain(Pair(near), input, output, Arc::clone(&abort)),
            far,
            abort,
        )
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn retained_output_is_charged_and_refused_before_the_socket() {
        const TURNS: usize = 8;
        let input = Input::validation_checks(
            super::super::ValidationBudget::test_turns(TURNS as u64).unwrap(),
        );
        let window = input.window().unwrap();
        let (mut wire, mut far, abort) = pair(input, TURNS * 64 * 1024);
        window.begin().unwrap();
        window.output(64 * 1024 - 2).unwrap();
        wire.write_all(b"ab", later()).unwrap();
        assert_eq!(wire.output_left, TURNS * 64 * 1024 - 2);
        let mut two = [0; 2];
        far.read_exact(&mut two).unwrap();
        assert_eq!(&two, b"ab");
        window.finish().unwrap();
        window.begin().unwrap();
        window.output(64 * 1024 - 1).unwrap();
        // Refused before the socket: nothing more reaches the peer.
        assert_eq!(wire.write_all(b"ab", later()), Err(StreamError::Failed));
        assert!(abort.load(Ordering::Acquire));
        far.set_nonblocking(true).unwrap();
        assert!(far.read(&mut two).is_err());
        assert!(window.finish().is_err());
        assert!(window.begin().is_err());
        assert_eq!(wire.read(&mut two, later()), Err(StreamError::Failed));
    }

    #[test]
    fn the_connection_output_allowance_refuses_a_larger_request() {
        let (mut wire, _far, abort) = pair(Input::new(), 4);
        assert_eq!(wire.write_all(b"abcde", later()), Err(StreamError::Failed));
        assert!(abort.load(Ordering::Acquire));
    }

    #[test]
    fn hostile_input_and_an_abort_end_the_stream() {
        let (mut wire, mut far, abort) = pair(Input::new(), 64 * 1024);
        // Trust authentication: an AuthenticationOk before any SASL.
        far.write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0]).unwrap();
        let mut buffer = [0; 64];
        assert_eq!(wire.read(&mut buffer, later()), Err(StreamError::Failed));
        assert!(abort.load(Ordering::Acquire));
        assert_eq!(wire.write_all(b"x", later()), Err(StreamError::Failed));

        // The worker's abort stops a healthy stream before the socket.
        let (mut wire, mut far, abort) = pair(Input::new(), 64 * 1024);
        abort.store(true, Ordering::Release);
        assert_eq!(wire.write_all(b"x", later()), Err(StreamError::Failed));
        far.set_nonblocking(true).unwrap();
        assert!(far.read(&mut buffer).is_err());
    }
}
