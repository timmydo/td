//! The byte stream under td-postgres: the input filter and output windows
//! around the server's Unix socket.
use super::framing::Input;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_postgres::{Socket, Stream, StreamError};

/// Every byte td-postgres reads passes `Input` before the client parses
/// it, and every request is charged to the connection's output allowance
/// and the open window before it is written. A refusal, or the worker's
/// abort, fails the stream for good.
pub(crate) struct Wire<S = Socket> {
    stream: S,
    input: Input,
    output_left: usize,
    aborted: Arc<AtomicBool>,
}

impl<S: Stream> Wire<S> {
    pub(super) fn new(stream: S, input: Input, output: usize, abort: Arc<AtomicBool>) -> Self {
        Self {
            stream,
            input,
            output_left: output,
            aborted: abort,
        }
    }

    fn refuse(&mut self) -> StreamError {
        self.input.fail();
        self.aborted.store(true, Ordering::Release);
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
        let count = self.stream.read(buffer, deadline)?;
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
        self.stream.write_all(bytes, deadline)?;
        self.output_left -= bytes.len();
        if self.input.output(bytes.len()).is_err() {
            return Err(self.refuse());
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

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
            Wire::new(Pair(near), input, output, Arc::clone(&abort)),
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
