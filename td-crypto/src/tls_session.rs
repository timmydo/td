//! Exclusive socket-free TLS progress with consumed failure state.
use crate::{
    session_clock::SessionClock,
    tls_record::{Protection, Record, PLAINTEXT_LIMIT},
    ClientConfig, ServerConfig, TlsError, TlsProtocol,
};
use std::{
    io::{Cursor, Read, Write},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};
const PROTOCOL_OUTPUT_LIMIT: usize = 65_536;
const HANDSHAKE_OUTPUT_LIMIT: usize = PROTOCOL_OUTPUT_LIMIT + 18_437;
const ESTABLISHED_OUTPUT_LIMIT: usize = 2 * 18_437;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsVersion {
    V12,
    V13,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsPhase {
    Handshaking,
    Open,
    Closing,
    Closed,
    Failed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerEvidence {
    VerifiedServerName,
    Unauthenticated,
    VerifiedClientLeaf([u8; 32]),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HandshakeInfo {
    pub version: TlsVersion,
    pub peer: PeerEvidence,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlsStatus {
    pub phase: TlsPhase,
    pub plaintext_pending: usize,
    pub ciphertext_pending: usize,
    pub wants_input: bool,
    pub write_ready: bool,
    pub read_closed: bool,
    pub write_closed: bool,
    pub handshake: Option<HandshakeInfo>,
    pub error: Option<TlsError>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockedOn {
    PeerInput,
    DrainPlaintext,
    DrainCiphertext,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsProgress {
    Bytes(usize),
    Blocked(BlockedOn),
    Eof,
}
/// A single exclusive connection. On an error, discard any caller output and
/// all previously obtained handshake evidence; status/abort/drop remain usable.
pub struct TlsSession {
    live: Option<Live>,
    status: TlsStatus,
}
#[derive(Clone)]
enum Role {
    Client(Arc<ClientConfig>),
    Server(Arc<ServerConfig>),
}
struct Live {
    connection: Option<rustls::Connection>,
    acceptor: Option<rustls::server::Acceptor>,
    hellos: Option<crate::tls_hello::RawHellos>,
    selected: Option<usize>,
    clock: Arc<SessionClock>,
    config: Role,
    protection: Protection,
    finished_flight_drained: bool,
    status: TlsStatus,
}
impl TlsSession {
    pub fn client(config: Arc<ClientConfig>, name: &str) -> Result<Self, TlsError> {
        crate::identity::validate_name(name)?;
        // All mutable native connection state is fresh and consumed by this
        // boundary. Shared policy is immutable; its clock has its own fence.
        let live = catch_unwind(AssertUnwindSafe(move || Live::client(config, name)))
            .map_err(|_| TlsError::Crypto)??;
        Ok(Self {
            status: live.status,
            live: Some(live),
        })
    }
    pub fn server(config: Arc<ServerConfig>) -> Result<Self, TlsError> {
        let live = catch_unwind(AssertUnwindSafe(move || Live::server(config)))
            .map_err(|_| TlsError::Crypto)??;
        Ok(Self {
            status: live.status,
            live: Some(live),
        })
    }
    pub fn status(&self) -> TlsStatus {
        self.status
    }
    pub fn receive_record(
        &mut self,
        wire: &[u8],
        socket_unwritten: bool,
    ) -> Result<TlsProgress, TlsError> {
        self.run(|live| live.receive(wire, socket_unwritten))
    }
    pub fn drain_ciphertext(&mut self, output: &mut [u8]) -> Result<usize, TlsError> {
        self.run(|live| {
            if output.is_empty() || live.status.ciphertext_pending == 0 {
                return Ok(0);
            }
            let count = live.backend(|connection| {
                connection
                    .write_tls(&mut Cursor::new(output))
                    .map_err(|_| TlsError::Crypto)
            })?;
            if count == 0 {
                return Err(TlsError::Crypto);
            }
            live.status.ciphertext_pending = live
                .status
                .ciphertext_pending
                .checked_sub(count)
                .ok_or(TlsError::Crypto)?;
            Ok(count)
        })
    }
    pub fn read_plaintext(&mut self, output: &mut [u8]) -> Result<TlsProgress, TlsError> {
        self.run(|live| {
            if live.status.handshake.is_none() {
                return Ok(live.handshake_blocked());
            }
            if live.status.read_closed && live.status.plaintext_pending == 0 {
                return Ok(TlsProgress::Eof);
            }
            if output.is_empty() {
                return Ok(TlsProgress::Bytes(0));
            }
            if live.status.plaintext_pending == 0 {
                return Ok(if live.status.read_closed {
                    TlsProgress::Eof
                } else {
                    TlsProgress::Blocked(BlockedOn::PeerInput)
                });
            }
            let count = live.backend(|connection| {
                connection
                    .reader()
                    .read(output)
                    .map_err(|_| TlsError::Crypto)
            })?;
            if count == 0 {
                return Err(TlsError::Crypto);
            }
            live.status.plaintext_pending = live
                .status
                .plaintext_pending
                .checked_sub(count)
                .ok_or(TlsError::Crypto)?;
            Ok(TlsProgress::Bytes(count))
        })
    }
    pub fn queue_plaintext(&mut self, input: &[u8]) -> Result<TlsProgress, TlsError> {
        self.run(|live| {
            if live.status.write_closed {
                return Err(TlsError::Invalid);
            }
            if live.status.handshake.is_none() {
                return Ok(live.handshake_blocked());
            }
            if input.is_empty() {
                return Ok(TlsProgress::Bytes(0));
            }
            if live.status.ciphertext_pending != 0 {
                return Ok(TlsProgress::Blocked(BlockedOn::DrainCiphertext));
            }
            let input = input
                .get(..input.len().min(PLAINTEXT_LIMIT))
                .ok_or(TlsError::Crypto)?;
            let count = live.backend(|connection| {
                connection
                    .writer()
                    .write(input)
                    .map_err(|_| TlsError::Crypto)
            })?;
            if count == 0 || count > input.len() {
                return Err(TlsError::Crypto);
            }
            live.process()?;
            Ok(TlsProgress::Bytes(count))
        })
    }
    /// Closing before Finished is terminal Invalid; use abort to cancel a handshake.
    pub fn close(&mut self) -> Result<(), TlsError> {
        self.run(|live| {
            if live.status.write_closed {
                return Ok(());
            }
            if live.status.handshake.is_none() {
                return Err(TlsError::Invalid);
            }
            live.close_write()
        })
    }
    pub fn transport_eof(&mut self) -> Result<(), TlsError> {
        self.run(|live| {
            if live.status.read_closed && live.status.handshake.is_some() {
                Ok(())
            } else {
                Err(TlsError::Protocol)
            }
        })
    }
    pub fn abort(&mut self) {
        if self.status.phase != TlsPhase::Failed {
            self.fail(TlsError::Invalid);
        }
    }
    fn fail(&mut self, error: TlsError) {
        self.live = None;
        self.status = TlsStatus {
            phase: TlsPhase::Failed,
            plaintext_pending: 0,
            ciphertext_pending: 0,
            wants_input: false,
            write_ready: false,
            read_closed: true,
            write_closed: true,
            handshake: None,
            error: Some(error),
        };
    }
    fn run<T>(
        &mut self,
        operation: impl FnOnce(&mut Live) -> Result<T, TlsError>,
    ) -> Result<T, TlsError> {
        if let Some(error) = self.status.error {
            return Err(error);
        }
        let Some(mut live) = self.live.take() else {
            self.fail(TlsError::Crypto);
            return Err(TlsError::Crypto);
        };
        // On unwind the consumed connection is dropped, never restored. Slices
        // borrowed by the operation carry no invariant and are discarded on error.
        let result = catch_unwind(AssertUnwindSafe(move || {
            let now = live.clock.now()?;
            live.check_material(now)?;
            let result = operation(&mut live)?;
            live.clock.check()?;
            if matches!(live.config, Role::Server(_)) {
                live.check_material(live.clock.now()?)?;
            }
            live.publish()?;
            Ok((live, result))
        }))
        .map_err(|_| TlsError::Crypto)
        .and_then(|result| result);
        match result {
            Ok((live, value)) => {
                self.status = live.status;
                self.live = Some(live);
                Ok(value)
            }
            Err(error) => {
                self.fail(error);
                Err(error)
            }
        }
    }
}
impl std::fmt::Debug for TlsSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsSession")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
impl Live {
    fn client(config: Arc<ClientConfig>, name: &str) -> Result<Self, TlsError> {
        let clock = Arc::new(SessionClock::new(config.clock.clone()));
        clock.now()?;
        let mut native = (*config.native).clone();
        native.time_provider = clock.clone();
        let mut owned_name = String::new();
        owned_name
            .try_reserve_exact(name.len())
            .map_err(|_| TlsError::Capacity)?;
        owned_name.push_str(name);
        let name =
            rustls::pki_types::ServerName::try_from(owned_name).map_err(|_| TlsError::Invalid)?;
        let connection =
            rustls::ClientConnection::new(Arc::new(native), name).map_err(native_error);
        clock.check()?;
        let mut live = Self {
            connection: Some(rustls::Connection::Client(connection?)),
            acceptor: None,
            hellos: None,
            selected: None,
            clock,
            config: Role::Client(config),
            protection: Protection::Plain,
            finished_flight_drained: false,
            status: TlsStatus {
                phase: TlsPhase::Handshaking,
                plaintext_pending: 0,
                ciphertext_pending: 0,
                wants_input: false,
                write_ready: false,
                read_closed: false,
                write_closed: false,
                handshake: None,
                error: None,
            },
        };
        live.backend(|connection| {
            connection.set_buffer_limit(Some(32 * 1024));
            Ok(())
        })?;
        live.process()?;
        live.publish()?;
        Ok(live)
    }
    fn backend<T>(
        &mut self,
        operation: impl FnOnce(&mut rustls::Connection) -> Result<T, TlsError>,
    ) -> Result<T, TlsError> {
        let value = operation(self.connection.as_mut().ok_or(TlsError::Crypto)?);
        self.clock.check()?;
        self.check_selected_health()?;
        value
    }
    fn process(&mut self) -> Result<(), TlsError> {
        let mut io =
            self.backend(|connection| connection.process_new_packets().map_err(native_error))?;
        if !self.status.write_closed && !self.connection()?.is_handshaking() {
            // Flush a deferred KeyUpdate response without application bytes.
            // After local close this must remain suppressed with all new output.
            let count = self.backend(|connection| {
                connection.writer().write(&[]).map_err(|_| TlsError::Crypto)
            })?;
            if count != 0 {
                return Err(TlsError::Crypto);
            }
            io =
                self.backend(|connection| connection.process_new_packets().map_err(native_error))?;
        }
        if self.status.write_closed && io.tls_bytes_to_write() > self.status.ciphertext_pending {
            return Err(TlsError::Protocol);
        }
        let output_limit = if self.finished_flight_drained {
            ESTABLISHED_OUTPUT_LIMIT
        } else {
            HANDSHAKE_OUTPUT_LIMIT
        };
        if io.tls_bytes_to_write() > output_limit || io.plaintext_bytes_to_read() > PLAINTEXT_LIMIT
        {
            return Err(TlsError::Capacity);
        }
        self.status.ciphertext_pending = io.tls_bytes_to_write();
        self.status.plaintext_pending = io.plaintext_bytes_to_read();
        self.status.read_closed |= io.peer_has_closed();
        Ok(())
    }
    fn receive(&mut self, wire: &[u8], socket_unwritten: bool) -> Result<TlsProgress, TlsError> {
        let handshaking = self.status.handshake.is_none();
        let record = Record::parse(wire, self.protection, handshaking)?;
        if self.status.read_closed {
            return Ok(TlsProgress::Bytes(wire.len()));
        }
        if self.status.plaintext_pending != 0 {
            return Ok(TlsProgress::Blocked(BlockedOn::DrainPlaintext));
        }
        if handshaking && (self.status.ciphertext_pending != 0 || socket_unwritten) {
            return Ok(TlsProgress::Blocked(BlockedOn::DrainCiphertext));
        }
        if self.protection == Protection::Tls12
            && record.kind == 21
            && (self.status.ciphertext_pending != 0 || socket_unwritten)
        {
            return Err(TlsError::Protocol);
        }
        if let Some(hellos) = self.hellos.as_mut() {
            if record.kind == 22 && self.protection != Protection::Tls12 {
                hellos.feed(record.body)?;
            }
        }
        if self.acceptor.is_some() {
            return self.accept(wire);
        }
        let mut input = Cursor::new(wire);
        let length = u64::try_from(wire.len()).map_err(|_| TlsError::Protocol)?;
        while input.position() < length {
            let before = input.position();
            let count = self.backend(|connection| {
                connection
                    .read_tls(&mut input)
                    .map_err(|_| TlsError::Capacity)
            })?;
            if count == 0 || input.position() <= before {
                return Err(TlsError::Protocol);
            }
            self.process()?;
        }
        match self.connection()?.protocol_version() {
            Some(rustls::ProtocolVersion::TLSv1_3) => self.protection = Protection::Tls13,
            Some(rustls::ProtocolVersion::TLSv1_2) if record.kind == 20 && record.body == [1] => {
                self.protection = Protection::Tls12
            }
            _ => {}
        }
        if self.status.read_closed {
            if handshaking {
                return Err(TlsError::Protocol);
            }
            if self.protection == Protection::Tls12 && !self.status.write_closed {
                self.close_write()?;
            }
        }
        Ok(TlsProgress::Bytes(wire.len()))
    }
    fn close_write(&mut self) -> Result<(), TlsError> {
        let io = self.backend(|connection| {
            connection.send_close_notify();
            connection.process_new_packets().map_err(native_error)
        })?;
        // Only this locally requested close may extend the output baseline.
        self.status.ciphertext_pending = io.tls_bytes_to_write();
        self.status.write_closed = true;
        self.process()
    }
    fn handshake_blocked(&self) -> TlsProgress {
        TlsProgress::Blocked(if self.status.ciphertext_pending != 0 {
            BlockedOn::DrainCiphertext
        } else {
            BlockedOn::PeerInput
        })
    }
    fn publish(&mut self) -> Result<(), TlsError> {
        self.clock.check()?;
        if self.connection.is_none() {
            self.status.wants_input = true;
            return self.clock.check();
        }
        if self.status.handshake.is_none() && !self.connection()?.is_handshaking() {
            if self.status.read_closed || self.status.write_closed {
                return Err(TlsError::Protocol);
            }
            if !matches!(
                self.connection()?.handshake_kind(),
                Some(
                    rustls::HandshakeKind::Full | rustls::HandshakeKind::FullWithHelloRetryRequest
                )
            ) {
                return Err(TlsError::Crypto);
            }
            let version = match self.connection()?.protocol_version() {
                Some(rustls::ProtocolVersion::TLSv1_2) => TlsVersion::V12,
                Some(rustls::ProtocolVersion::TLSv1_3) => TlsVersion::V13,
                _ => return Err(TlsError::Crypto),
            };
            let valid_alpn = matches!(
                (self.protocol(), self.connection()?.alpn_protocol()),
                (_, None) | (TlsProtocol::Http1, Some(b"http/1.1"))
            );
            if !valid_alpn {
                return Err(TlsError::Protocol);
            }
            let peer = self.peer_evidence()?;
            self.status.handshake = Some(HandshakeInfo { version, peer });
        }
        // Finished may still leave a large final handshake flight to drain.
        self.finished_flight_drained |=
            self.status.handshake.is_some() && self.status.ciphertext_pending == 0;
        self.status.wants_input = !self.status.read_closed
            && self.status.plaintext_pending == 0
            && (self.status.handshake.is_some() || self.status.ciphertext_pending == 0);
        self.status.write_ready = !self.status.write_closed
            && self.status.handshake.is_some()
            && self.status.ciphertext_pending == 0;
        self.status.phase = if self.status.read_closed
            && self.status.write_closed
            && self.status.ciphertext_pending == 0
        {
            TlsPhase::Closed
        } else if self.status.read_closed || self.status.write_closed {
            TlsPhase::Closing
        } else if self.status.handshake.is_some() {
            TlsPhase::Open
        } else {
            TlsPhase::Handshaking
        };
        self.check_selected_health()?;
        self.clock.check()
    }
}
#[path = "tls_server_session.rs"]
mod server;

fn native_error(error: rustls::Error) -> TlsError {
    use crate::VerificationFailure as V;
    use rustls::{CertificateError as C, Error as E};
    match error {
        E::NoCertificatesPresented => TlsError::Verification(V::Missing),
        E::InvalidCertificate(error) => TlsError::Verification(match error {
            C::Expired | C::ExpiredContext { .. } => V::Expired,
            C::NotValidYet | C::NotValidYetContext { .. } => V::NotYetValid,
            C::UnknownIssuer => V::Untrusted,
            C::BadSignature => V::Signature,
            C::NotValidForName | C::NotValidForNameContext { .. } => V::Name,
            C::InvalidPurpose | C::InvalidPurposeContext { .. } => V::Usage,
            _ => V::Other,
        }),
        E::InvalidMessage(
            rustls::InvalidMessage::CertificatePayloadTooLarge
            | rustls::InvalidMessage::HandshakePayloadTooLarge,
        ) => TlsError::Capacity,
        E::FailedToGetCurrentTime => TlsError::Clock,
        E::Other(rustls::OtherError(error)) => error
            .downcast_ref::<TlsError>()
            .copied()
            .unwrap_or(TlsError::Crypto),
        E::EncryptError
        | E::BadMaxFragmentSize
        | E::HandshakeNotComplete
        | E::FailedToGetRandomBytes
        | E::General(_) => TlsError::Crypto,
        _ => TlsError::Protocol,
    }
}

#[cfg(test)]
#[path = "tls_session_tests.rs"]
mod tests;
