//! Initial EHLO and required STARTTLS offer stay with their original socket.
use super::{
    Binding, ClientStartTls, SessionRefusal, SessionReservation, TlsPolicies, UpgradeScratch,
};
use crate::{
    config::outbound,
    ports::{Clock, Deadline, Error, Transport},
    smtp_ehlo,
    smtp_wire::StartTlsOffer,
    tls_io::TlsWireStorage,
    transport::TcpTransport,
};
use std::sync::Arc;

pub use smtp_ehlo::Scratch as RelayEhloScratch;

#[must_use = "drive the initial EHLO or recover its reserved buffers"]
pub struct RelayStartTls<'a, B: TlsWireStorage> {
    inner: smtp_ehlo::Exchange<'a, TcpTransport>,
    prepared: SessionReservation<B>,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    handshake_deadline: Deadline,
}

#[must_use = "continue EHLO or upgrade the same retained socket"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline socket and TLS custody avoids per-turn allocation."
)]
pub enum RelayStartTlsProgress<'a, B: TlsWireStorage> {
    Pending(RelayStartTls<'a, B>),
    Ready(RelayStartTlsReady<B>),
}

/// A parsed offer is not peer verification. This owner exposes no plaintext
/// socket or capabilities; upgrade consumes the same socket and reservation.
pub struct RelayStartTlsReady<B: TlsWireStorage> {
    plain: TcpTransport,
    offer: StartTlsOffer,
    prepared: SessionReservation<B>,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    handshake_deadline: Deadline,
}

impl<'a, B: TlsWireStorage> RelayStartTls<'a, B> {
    /// Caller supplies the configured endpoint, sole outbound slot, clock origin
    /// and original deadlines. No credential or durable queue authority is given.
    pub fn new(
        prepared: SessionReservation<B>,
        plain: TcpTransport,
        hostname: &str,
        scratch: RelayEhloScratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        handshake_deadline: Deadline,
    ) -> Result<Self, SessionRefusal<B>> {
        let init = (|| {
            if handshake_deadline.tick() > deadline.tick() {
                return Err(Error::Invalid);
            }
            let policy = TlsPolicies::resolve(&prepared.lease, prepared.id)?;
            if !matches!(
                policy.binding,
                Binding::Relay {
                    transport: outbound::Transport::RequiredStartTls,
                    ..
                }
            ) {
                return Err(Error::Invalid);
            }
            Ok(())
        })();
        if let Err(error) = init {
            return Err(refuse(plain, prepared, error));
        }
        match smtp_ehlo::Exchange::new(
            plain,
            hostname,
            scratch,
            clock.clone(),
            handshake_deadline,
            true,
        ) {
            Ok(inner) => Ok(Self {
                inner,
                prepared,
                clock,
                deadline,
                handshake_deadline,
            }),
            Err(failure) => Err(refuse(failure.connection, prepared, failure.error)),
        }
    }
    /// One control line and at most one socket operation under the original
    /// handshake deadline, which may not exceed the original job deadline.
    pub fn advance(self) -> Result<RelayStartTlsProgress<'a, B>, SessionRefusal<B>> {
        let Self {
            inner,
            prepared,
            clock,
            deadline,
            handshake_deadline,
        } = self;
        match inner.advance() {
            Err(failure) => Err(refuse(failure.connection, prepared, failure.error)),
            Ok(smtp_ehlo::Progress::Pending(inner)) => Ok(RelayStartTlsProgress::Pending(Self {
                inner,
                prepared,
                clock,
                deadline,
                handshake_deadline,
            })),
            Ok(smtp_ehlo::Progress::Ready(plain, ehlo)) => match ehlo.into_starttls_offer(&[]) {
                Ok(offer) => Ok(RelayStartTlsProgress::Ready(RelayStartTlsReady {
                    plain,
                    offer,
                    prepared,
                    clock,
                    deadline,
                    handshake_deadline,
                })),
                Err(error) => Err(refuse(plain, prepared, error)),
            },
        }
    }
    pub fn into_buffers(self) -> (B, B) {
        let mut plain = self.inner.into_connection();
        plain.abort();
        self.prepared.into_buffers()
    }
}
impl<B: TlsWireStorage> RelayStartTlsReady<B> {
    /// Reuse the released EHLO cells for the existing bounded 220/TLS driver.
    /// Plaintext AUTH/8BITMIME advertisements are discarded; TLS requires a new
    /// verified EHLO before either can authorize a relay operation.
    pub fn upgrade(
        self,
        scratch: UpgradeScratch<'_>,
    ) -> Result<ClientStartTls<'_, B>, SessionRefusal<B>> {
        ClientStartTls::new(
            self.offer,
            self.prepared,
            self.plain,
            scratch,
            self.clock,
            self.deadline,
            self.handshake_deadline,
        )
    }
    pub fn into_buffers(mut self) -> (B, B) {
        self.plain.abort();
        self.prepared.into_buffers()
    }
}
fn refuse<B: TlsWireStorage>(
    mut plain: TcpTransport,
    prepared: SessionReservation<B>,
    error: Error,
) -> SessionRefusal<B> {
    plain.abort();
    let (input, output) = prepared.into_buffers();
    SessionRefusal {
        error,
        input,
        output,
    }
}
const _: () = assert!(
    std::mem::size_of::<RelayStartTlsProgress<'static, Box<[u8; crate::tls_io::TLS_WIRE_BYTES]>>>()
        <= 4096
);
