//! Receiving STARTTLS ownership from an exact command through a real handshake.
use crate::{
    ports::{Clock, Deadline, Error, Handshake, TlsTransport},
    smtp_network::Network,
    smtp_wire::{LineReader, LINE_BYTES},
    tls_io::TlsWireStorage,
    tls_policy::{ServerStartTls, ServerUpgradeProgress, SessionReservation, TlsConnection},
    transport::TcpTransport,
};
use std::sync::Arc;

enum State<B: TlsWireStorage> {
    Reply(ServerStartTls<B>),
    Handshake(TlsConnection<B>),
}

/// Move the admitted connection to a TLS worker with its prepared native
/// session. No plaintext connection can be recovered after starting upgrade.
/// Drive or drop this owner on that worker, including native teardown.
#[must_use = "drive or retire the upgrade on a TLS worker"]
pub struct Upgrade<'a, B: TlsWireStorage> {
    network: Network<'a>,
    state: State<B>,
    clock: Arc<dyn Clock>,
    handshake_deadline: Deadline,
}

#[must_use = "continue the upgrade or schedule the established connection"]
pub enum Progress<'a, B: TlsWireStorage> {
    Pending(Upgrade<'a, B>),
    Established {
        network: Network<'a>,
        transport: TlsConnection<B>,
    },
}

impl<'a, B: TlsWireStorage> Upgrade<'a, B> {
    /// Supply the same exclusively owned socket used to drive this network.
    /// Native construction and handshake capacity admission precede this call.
    /// The original handshake deadline includes preparation and queue time;
    /// it must fit within the connection's unchanged enclosing deadline.
    /// Refusal consumes socket and native state; borrowed buffers are released
    /// and owned buffers are freed. It never returns a plaintext fallback.
    pub fn new(
        mut network: Network<'a>,
        plain: TcpTransport,
        prepared: SessionReservation<B>,
        clock: Arc<dyn Clock>,
        handshake_deadline: Deadline,
    ) -> Result<Self, Error> {
        let command = network.check_starttls(clock.as_ref())?;
        let mut scratch = [0; LINE_BYTES];
        let mut reader = LineReader::new(&mut scratch)?;
        let parsed = reader.feed(command)?;
        if !parsed.complete || parsed.consumed != command.len() {
            return Err(Error::Invalid);
        }
        if handshake_deadline > network.deadline() {
            return Err(Error::Invalid);
        }
        let deadline = network.transport_deadline();
        let upgrade = ServerStartTls::new(
            prepared,
            plain,
            &reader,
            &[],
            clock.clone(),
            deadline,
            handshake_deadline,
        )
        .map_err(|refusal| refusal.error())?;
        Ok(Self {
            network,
            state: State::Reply(upgrade),
            clock,
            handshake_deadline,
        })
    }

    /// One bounded reply or handshake turn. SMTP state resets only after the
    /// real connection reports a completed, policy-authorized handshake.
    pub fn advance(mut self) -> Result<Progress<'a, B>, Error> {
        self.network.check_starttls(self.clock.as_ref())?;
        match self.state {
            State::Reply(upgrade) => {
                self.state = match upgrade.advance().map_err(|refusal| refusal.error())? {
                    ServerUpgradeProgress::Pending(upgrade) => State::Reply(upgrade),
                    ServerUpgradeProgress::Tls(transport) => State::Handshake(transport),
                };
                Ok(Progress::Pending(self))
            }
            State::Handshake(mut transport) => {
                match transport.handshake(self.handshake_deadline)? {
                    Handshake::Pending => {
                        self.state = State::Handshake(transport);
                        Ok(Progress::Pending(self))
                    }
                    Handshake::Complete(_) => {
                        self.network.check_starttls(self.clock.as_ref())?;
                        self.network.tls_established(self.clock.as_ref())?;
                        Ok(Progress::Established {
                            network: self.network,
                            transport,
                        })
                    }
                }
            }
        }
    }
}
