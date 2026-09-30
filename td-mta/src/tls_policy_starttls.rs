//! The inbound STARTTLS reply boundary, after command parsing and reservation.
use super::{PolicyRole, SessionRefusal, SessionReservation, TlsConnection, TlsPolicies};
use crate::{
    ports::{Clock, Deadline, Error, FlushProgress, IoProgress, Transport},
    smtp_wire::LineReader,
    tls_io::TlsWireStorage,
    transport::TcpTransport,
};
use std::sync::Arc;

const READY: &[u8] = b"220 2.0.0 Ready to start TLS\r\n";

/// Owns the prepared native session before sending any 220 bytes. Protocol state
/// reset after handshake success remains the SMTP driver's responsibility.
#[must_use = "drive the upgrade or recover its reserved buffers"]
pub struct ServerStartTls<B: TlsWireStorage> {
    plain: TcpTransport,
    prepared: SessionReservation<B>,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    handshake_deadline: Deadline,
    reply: ReplyFlush,
}

#[must_use = "continue progress or drive the returned TLS handshake"]
pub enum ServerUpgradeProgress<B: TlsWireStorage> {
    Pending(ServerStartTls<B>),
    /// The 220 is flushed; the TLS handshake has not yet completed.
    Tls(TlsConnection<B>),
}

impl<B: TlsWireStorage> ServerStartTls<B> {
    /// Use the completed control reader and its exact unread tail from this
    /// socket. The trusted driver must not hold other plaintext input/output.
    /// Native construction happens on a TLS worker before entering this owner.
    pub fn new(
        prepared: SessionReservation<B>,
        plain: TcpTransport,
        command: &LineReader<'_>,
        tail: &[u8],
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        handshake_deadline: Deadline,
    ) -> Result<Self, SessionRefusal<B>> {
        let owner = Self {
            plain,
            prepared,
            clock,
            deadline,
            handshake_deadline,
            reply: ReplyFlush { written: 0 },
        };
        let valid = (|| {
            if !tail.is_empty()
                || !command
                    .line()
                    .is_some_and(|line| line.eq_ignore_ascii_case(b"STARTTLS"))
                || handshake_deadline.tick() > deadline.tick()
            {
                return Err(Error::Invalid);
            }
            let policy = TlsPolicies::resolve(&owner.prepared.lease, owner.prepared.id)?;
            if !matches!(
                policy.role()?,
                PolicyRole::DirectSmtp | PolicyRole::GatewaySmtp
            ) {
                return Err(Error::Invalid);
            }
            owner.check_time()
        })();
        match valid {
            Ok(()) => Ok(owner),
            Err(error) => Err(owner.refuse(error)),
        }
    }

    /// One bounded write or flush. Ownership returns through Pending so failure
    /// always consumes and tears down the socket/native session before refusal.
    pub fn advance(mut self) -> Result<ServerUpgradeProgress<B>, SessionRefusal<B>> {
        let progress = (|| {
            self.check_time()?;
            let progress = self.reply.advance(&mut self.plain)?;
            self.check_time()?;
            Ok(progress)
        })();
        match progress {
            Err(error) => Err(self.refuse(error)),
            Ok(FlushProgress::Pending) => Ok(ServerUpgradeProgress::Pending(self)),
            Ok(FlushProgress::Complete) => {
                let Self {
                    plain,
                    prepared,
                    clock,
                    deadline,
                    handshake_deadline,
                    ..
                } = self;
                prepared
                    .handoff(plain, &[], clock, deadline, handshake_deadline)
                    .map(ServerUpgradeProgress::Tls)
            }
        }
    }

    pub fn into_buffers(mut self) -> (B, B) {
        self.plain.abort();
        self.prepared.into_buffers()
    }

    fn check_time(&self) -> Result<(), Error> {
        let now = self.clock.sample()?.monotonic;
        if self.deadline.expired(now) || self.handshake_deadline.expired(now) {
            Err(Error::Deadline)
        } else {
            Ok(())
        }
    }

    fn refuse(self, error: Error) -> SessionRefusal<B> {
        let (input, output) = self.into_buffers();
        SessionRefusal {
            error,
            input,
            output,
        }
    }
}

struct ReplyFlush {
    written: usize,
}
impl ReplyFlush {
    fn advance(&mut self, stream: &mut impl Transport) -> Result<FlushProgress, Error> {
        let remaining = READY.get(self.written..).ok_or(Error::Invalid)?;
        if remaining.is_empty() {
            return stream.flush();
        }
        match stream.write(remaining)? {
            IoProgress::Bytes(count) if count > 0 && count <= remaining.len() => {
                self.written = self.written.checked_add(count).ok_or(Error::Invalid)?;
                Ok(FlushProgress::Pending)
            }
            IoProgress::Pending => Ok(FlushProgress::Pending),
            _ => Err(Error::Invalid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Script {
        writes: VecDeque<Result<IoProgress, Error>>,
        flushes: VecDeque<Result<FlushProgress, Error>>,
        output: Vec<u8>,
    }
    impl Transport for Script {
        fn read(&mut self, _: &mut [u8]) -> Result<IoProgress, Error> {
            Err(Error::Invalid)
        }
        fn write(&mut self, bytes: &[u8]) -> Result<IoProgress, Error> {
            let result = self.writes.pop_front().ok_or(Error::Conflict)??;
            if let IoProgress::Bytes(count) = result {
                if let Some(bytes) = bytes.get(..count) {
                    self.output.extend_from_slice(bytes);
                }
            }
            Ok(result)
        }
        fn flush(&mut self) -> Result<FlushProgress, Error> {
            if self.output != READY {
                return Err(Error::Conflict);
            }
            self.flushes.pop_front().ok_or(Error::Conflict)?
        }
        fn close(&mut self) -> Result<FlushProgress, Error> {
            Err(Error::Conflict)
        }
        fn abort(&mut self) {}
    }

    #[test]
    fn starttls_reply_requires_short_writes_and_complete_flush() -> Result<(), Error> {
        let mut stream = Script {
            writes: [
                Ok(IoProgress::Pending),
                Ok(IoProgress::Bytes(1)),
                Ok(IoProgress::Bytes(READY.len() - 1)),
            ]
            .into(),
            flushes: [Ok(FlushProgress::Pending), Ok(FlushProgress::Complete)].into(),
            output: Vec::new(),
        };
        let mut reply = ReplyFlush { written: 0 };
        for _ in 0..4 {
            assert_eq!(reply.advance(&mut stream), Ok(FlushProgress::Pending));
        }
        assert_eq!(reply.advance(&mut stream), Ok(FlushProgress::Complete));
        assert_eq!(stream.output, READY);
        assert!(stream.writes.is_empty());
        assert!(stream.flushes.is_empty());
        Ok(())
    }

    #[test]
    fn starttls_reply_refuses_invalid_write_counts_and_flush_failure() {
        for result in [
            Ok(IoProgress::Bytes(0)),
            Ok(IoProgress::Bytes(READY.len() + 1)),
            Ok(IoProgress::Closed),
            Err(Error::Busy),
        ] {
            let mut stream = Script {
                writes: [result].into(),
                flushes: VecDeque::new(),
                output: Vec::new(),
            };
            let mut reply = ReplyFlush { written: 0 };
            assert_eq!(
                reply.advance(&mut stream),
                Err(if result == Err(Error::Busy) {
                    Error::Busy
                } else {
                    Error::Invalid
                })
            );
        }
        let mut stream = Script {
            writes: [Ok(IoProgress::Bytes(READY.len()))].into(),
            flushes: [Err(Error::Busy)].into(),
            output: Vec::new(),
        };
        let mut reply = ReplyFlush { written: 0 };
        assert_eq!(reply.advance(&mut stream), Ok(FlushProgress::Pending));
        assert_eq!(reply.advance(&mut stream), Err(Error::Busy));
    }
}
