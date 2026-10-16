//! Fresh EHLO capability custody on one verified configured relay connection.
use crate::{
    config::outbound,
    ports::{Clock, Deadline, Error, TlsPolicyId, Transport},
    smtp_wire::{AuthMechanisms, RelayCapabilities},
    tls_io::TlsWireStorage,
    tls_policy::TlsConnection,
};
use std::sync::Arc;

use crate::smtp_ehlo;
pub use smtp_ehlo::Scratch;

#[must_use = "drive the EHLO exchange or recover its reserved buffers"]
pub struct Exchange<'a, B: TlsWireStorage> {
    inner: smtp_ehlo::Exchange<'a, TlsConnection<B>>,
}
#[must_use = "continue the exchange or retain the ready relay session"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn heap allocation."
)]
pub enum Progress<'a, B: TlsWireStorage> {
    Pending(Exchange<'a, B>),
    Ready(Session<B>),
}

/// The completed post-handshake EHLO and its exclusive verified connection.
/// This is not account/queue authority or proof that authentication succeeded.
pub struct Session<B: TlsWireStorage> {
    connection: TlsConnection<B>,
    capabilities: RelayCapabilities,
}
impl<B: TlsWireStorage> Session<B> {
    pub fn mechanisms(&self) -> AuthMechanisms {
        self.capabilities.auth()
    }
    pub fn capabilities(&self) -> RelayCapabilities {
        self.capabilities
    }
    /// Match exact credentials to this retained relay policy. No AUTH command,
    /// current-generation admission or successful authentication is conferred.
    pub fn check_credentials(&self, username: &str, password: &str) -> Result<(), Error> {
        self.connection.check_relay_credentials(username, password)
    }
    pub fn policy_id(&self) -> TlsPolicyId {
        self.connection.policy_id()
    }
    pub(crate) fn into_connection(self) -> TlsConnection<B> {
        self.connection
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.connection.into_buffers()
    }
}

/// Refusal has torn down TLS and the socket; only reserved buffers remain.
pub struct Refusal<B: TlsWireStorage> {
    pub error: Error,
    buffers: Result<(B, B), Error>,
}
impl<B: TlsWireStorage> Refusal<B> {
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.buffers
    }
}

impl<'a, B: TlsWireStorage> Exchange<'a, B> {
    /// Accept only a completed configured-relay TLS handshake. Implicit TLS
    /// consumes the initial 220; required STARTTLS begins with a fresh EHLO.
    /// Supply the connection clock origin and the original exchange deadline.
    pub fn new(
        connection: TlsConnection<B>,
        hostname: &str,
        scratch: Scratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
    ) -> Result<Self, Refusal<B>> {
        let transport = match connection.verified_relay_transport() {
            Ok(transport) => transport,
            Err(error) => return Err(refuse(connection, error)),
        };
        smtp_ehlo::Exchange::new(
            connection,
            hostname,
            scratch,
            clock,
            deadline,
            transport == outbound::Transport::ImplicitTls,
        )
        .map(|inner| Self { inner })
        .map_err(|failure| refuse(failure.connection, failure.error))
    }
    /// One control-line parse and at most one TLS operation per turn.
    pub fn advance(self) -> Result<Progress<'a, B>, Refusal<B>> {
        match self.inner.advance() {
            Err(failure) => Err(refuse(failure.connection, failure.error)),
            Ok(smtp_ehlo::Progress::Pending(inner)) => Ok(Progress::Pending(Self { inner })),
            Ok(smtp_ehlo::Progress::Ready(connection, ehlo)) => {
                match ehlo.into_relay_capabilities(&[]) {
                    Ok(capabilities) => Ok(Progress::Ready(Session {
                        connection,
                        capabilities,
                    })),
                    Err(error) => Err(refuse(connection, error)),
                }
            }
        }
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.inner.into_connection().into_buffers()
    }
}
fn refuse<B: TlsWireStorage>(mut connection: TlsConnection<B>, error: Error) -> Refusal<B> {
    connection.abort();
    Refusal {
        error,
        buffers: connection.into_buffers(),
    }
}

const _: () = assert!(
    std::mem::size_of::<Progress<'static, Box<[u8; crate::tls_io::TLS_WIRE_BYTES]>>>() <= 4096
);
