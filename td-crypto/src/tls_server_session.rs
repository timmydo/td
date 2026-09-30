//! Initial server admission before signing, inside the common consuming boundary.
use super::*;
use crate::{Digest, Sha256};

impl Live {
    pub(super) fn connection(&self) -> Result<&rustls::Connection, TlsError> {
        self.connection.as_ref().ok_or(TlsError::Crypto)
    }
    pub(super) fn protocol(&self) -> TlsProtocol {
        match &self.config {
            Role::Client(config) => config.protocol,
            Role::Server(config) => config.protocol,
        }
    }
    fn selected_identity(&self) -> Result<Option<&crate::ServerIdentity>, TlsError> {
        match (&self.config, self.selected) {
            (Role::Server(config), Some(index)) => config
                .routing
                .identities
                .get(index)
                .map(|identity| Some(identity.as_ref()))
                .ok_or(TlsError::Crypto),
            (Role::Server(_), None) | (Role::Client(_), None) => Ok(None),
            _ => Err(TlsError::Crypto),
        }
    }
    pub(super) fn check_material(&self, now: u64) -> Result<(), TlsError> {
        if let Some(identity) = self.selected_identity()? {
            if self.status.handshake.is_none() {
                identity.check_validity(Some(now))?;
            } else {
                identity.check_health()?;
            }
        }
        Ok(())
    }
    pub(super) fn check_selected_health(&self) -> Result<(), TlsError> {
        if let Some(identity) = self.selected_identity()? {
            identity.check_health()?;
        }
        Ok(())
    }
    pub(super) fn server(config: Arc<ServerConfig>) -> Result<Self, TlsError> {
        let clock = Arc::new(SessionClock::new(config.clock.clone()));
        let now = clock.now()?;
        if config.identity_count() == 1 {
            config
                .routing
                .identities
                .first()
                .ok_or(TlsError::Crypto)?
                .check_validity(Some(now))?;
        }
        let mut live = Self {
            connection: None,
            acceptor: Some(rustls::server::Acceptor::default()),
            hellos: Some(crate::tls_hello::RawHellos::new()),
            selected: None,
            clock,
            config: Role::Server(config),
            protection: Protection::Plain,
            finished_flight_drained: false,
            status: TlsStatus {
                phase: TlsPhase::Handshaking,
                plaintext_pending: 0,
                ciphertext_pending: 0,
                wants_input: true,
                write_ready: false,
                read_closed: false,
                write_closed: false,
                handshake: None,
                error: None,
            },
        };
        live.publish()?;
        Ok(live)
    }
    pub(super) fn accept(&mut self, wire: &[u8]) -> Result<TlsProgress, TlsError> {
        let mut input = Cursor::new(wire);
        let length = u64::try_from(wire.len()).map_err(|_| TlsError::Protocol)?;
        while input.position() < length {
            let before = input.position();
            let acceptor = self.acceptor.as_mut().ok_or(TlsError::Crypto)?;
            let count = acceptor
                .read_tls(&mut input)
                .map_err(|_| TlsError::Capacity);
            self.clock.check()?;
            if count? == 0 || input.position() <= before {
                return Err(TlsError::Protocol);
            }
            let accepted = self.acceptor.as_mut().ok_or(TlsError::Crypto)?.accept();
            self.clock.check()?;
            if let Some(accepted) = accepted.map_err(|(error, _)| native_error(error))? {
                if input.position() != length {
                    return Err(TlsError::Protocol);
                }
                let Role::Server(config) = &self.config else {
                    return Err(TlsError::Crypto);
                };
                let raw_name = self.hellos.as_ref().ok_or(TlsError::Crypto)?.name()?;
                if raw_name != accepted.client_hello().server_name() {
                    return Err(TlsError::Protocol);
                }
                let selected = config.routing.select(raw_name)?;
                let now = self.clock.now()?;
                config
                    .routing
                    .identities
                    .get(selected)
                    .ok_or(TlsError::Crypto)?
                    .check_validity(Some(now))?;
                self.selected = Some(selected);
                let mut native = (*config.native).clone();
                native.time_provider = self.clock.clone();
                let connection = accepted.into_connection(Arc::new(native));
                self.clock.check()?;
                self.check_selected_health()?;
                self.connection = Some(rustls::Connection::Server(
                    connection.map_err(|(error, _)| native_error(error))?,
                ));
                self.acceptor = None;
                self.backend(|connection| {
                    connection.set_buffer_limit(Some(32 * 1024));
                    Ok(())
                })?;
                self.process()?;
                if self.connection()?.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3) {
                    self.protection = Protection::Tls13;
                }
                break;
            }
        }
        Ok(TlsProgress::Bytes(wire.len()))
    }
    pub(super) fn peer_evidence(&self) -> Result<PeerEvidence, TlsError> {
        match &self.config {
            Role::Client(_) => {
                if self
                    .connection()?
                    .peer_certificates()
                    .is_none_or(|chain| chain.is_empty())
                {
                    return Err(TlsError::Crypto);
                }
                Ok(PeerEvidence::VerifiedServerName)
            }
            Role::Server(config) if !config.mandatory_client_auth => {
                Ok(PeerEvidence::Unauthenticated)
            }
            Role::Server(_) => {
                let leaf = self
                    .connection()?
                    .peer_certificates()
                    .and_then(|chain| chain.first())
                    .ok_or(TlsError::Crypto)?;
                let mut hash = Sha256::try_new().map_err(|_| TlsError::Crypto)?;
                hash.update(leaf.as_ref()).map_err(|_| TlsError::Crypto)?;
                let digest = hash.finish().map_err(|_| TlsError::Crypto)?;
                Ok(PeerEvidence::VerifiedClientLeaf(digest))
            }
        }
    }
}
