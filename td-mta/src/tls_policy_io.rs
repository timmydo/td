//! Mail authorization over retained TLS state and an actual TCP peer.
use super::{Configuration, PolicyRole, SessionRefusal, SessionReservation, TlsPolicies};
use crate::{
    generations::GenerationLease,
    ports::{
        Clock, Deadline, Error, FlushProgress, Handshake, IoProgress, PeerVerification, TlsInfo,
        TlsPolicyId, TlsTransport, TlsVersion, Transport,
    },
    tls_admission::HandshakePermit,
    tls_io::{TlsIo, TlsWireStorage},
    transport::TcpTransport,
};
use std::{net::IpAddr, sync::Arc};
use td_crypto::{HandshakeInfo, PeerEvidence};

/// One retained policy and exclusively owned socket. Cached info is not a
/// current-generation mutation fence. No service endpoint uses this owner yet.
#[must_use = "drive the connection or recover its reserved buffers"]
pub struct TlsConnection<B: TlsWireStorage> {
    // Native teardown precedes material and count release on ordinary Drop.
    io: TlsIo<TcpTransport, B>,
    lease: GenerationLease<TlsPolicies>,
    permit: Option<HandshakePermit>,
    id: TlsPolicyId,
    peer: IpAddr,
    info: Option<TlsInfo>,
    failure: Option<Error>,
}

impl<B: TlsWireStorage> SessionReservation<B> {
    /// Consume the socket after STARTTLS framing/reply flush, or immediately
    /// for implicit TLS. Both deadlines and the clock use one runtime origin.
    /// Refusal aborts native/socket state before returning the original arrays.
    pub fn handoff(
        self,
        mut plain: TcpTransport,
        unconsumed_plaintext: &[u8],
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        handshake_deadline: Deadline,
    ) -> Result<TlsConnection<B>, SessionRefusal<B>> {
        if !unconsumed_plaintext.is_empty() {
            plain.abort();
            let (input, output) = self.into_buffers();
            return Err(SessionRefusal {
                error: Error::Invalid,
                input,
                output,
            });
        }
        let peer = plain.peer_addr().ip();
        let Self {
            session,
            lease,
            permit,
            id,
            input,
            output,
        } = self;
        match TlsIo::new(
            plain,
            session,
            clock,
            deadline,
            handshake_deadline,
            input,
            output,
        ) {
            Ok(io) => Ok(TlsConnection {
                io,
                lease,
                permit: Some(permit),
                id,
                peer,
                info: None,
                failure: None,
            }),
            Err(refusal) => {
                let error = refusal.error();
                let (input, output) = refusal.into_buffers();
                Err(SessionRefusal {
                    error,
                    input,
                    output,
                })
            }
        }
    }
}

impl<B: TlsWireStorage> TlsConnection<B> {
    pub const fn policy_id(&self) -> TlsPolicyId {
        self.id
    }

    pub fn into_buffers(self) -> Result<(B, B), Error> {
        let Self {
            io, lease, permit, ..
        } = self;
        let result = io.into_buffers();
        drop(lease);
        drop(permit);
        result
    }

    /// The trusted runtime supplies its actual current generation and serializes
    /// this check with publication/mutation. An old lease grants no freshness.
    pub fn check_gateway_policy(
        &mut self,
        current: &GenerationLease<TlsPolicies>,
    ) -> Result<(), Error> {
        self.live()?;
        let result = TlsPolicies::gateway_unchanged(&self.lease, self.id, current)
            .and_then(|same| if same { Ok(()) } else { Err(Error::Forbidden) });
        result.map_err(|error| self.fail(error))
    }

    fn live(&self) -> Result<(), Error> {
        match self.failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn fail(&mut self, error: Error) -> Error {
        let error = *self.failure.get_or_insert(error);
        self.info = None;
        self.io.abort();
        drop(self.permit.take());
        error
    }

    fn authorize(&self, evidence: HandshakeInfo) -> Result<TlsInfo, Error> {
        let policy = TlsPolicies::resolve(&self.lease, self.id)?;
        let peer = match (policy.role()?, evidence.peer) {
            (PolicyRole::DirectSmtp | PolicyRole::Https, PeerEvidence::Unauthenticated) => {
                PeerVerification::None
            }
            (PolicyRole::Relay | PolicyRole::Acme, PeerEvidence::VerifiedServerName) => {
                PeerVerification::ServerName
            }
            (PolicyRole::GatewaySmtp, PeerEvidence::VerifiedClientLeaf(hash)) => {
                let Configuration::Gateway(gateway) = &policy.configuration else {
                    return Err(Error::Invalid);
                };
                if !gateway.matches(self.peer, &hash)? {
                    return Err(Error::Forbidden);
                }
                PeerVerification::Gateway(hash)
            }
            _ => return Err(Error::Tls),
        };
        let version = match evidence.version {
            td_crypto::TlsVersion::V12 => TlsVersion::V12,
            td_crypto::TlsVersion::V13 => TlsVersion::V13,
        };
        Ok(TlsInfo { version, peer })
    }
}

impl<B: TlsWireStorage> TlsTransport for TlsConnection<B> {
    fn handshake(&mut self, deadline: Deadline) -> Result<Handshake, Error> {
        self.live()?;
        let result = self
            .io
            .handshake_before(deadline)
            .and_then(|evidence| match evidence {
                Some(evidence) => self.authorize(evidence).map(Handshake::Complete),
                None => Ok(Handshake::Pending),
            });
        match result {
            Ok(Handshake::Complete(info)) => {
                self.info = Some(info);
                drop(self.permit.take());
                Ok(Handshake::Complete(info))
            }
            Ok(Handshake::Pending) => Ok(Handshake::Pending),
            Err(error) => Err(self.fail(error)),
        }
    }
    fn info(&self) -> Option<TlsInfo> {
        self.info
    }
}

impl<B: TlsWireStorage> Transport for TlsConnection<B> {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, Error> {
        if output.is_empty() {
            return Ok(IoProgress::Pending);
        }
        self.live()?;
        let result = if self.info.is_some() {
            self.io.read(output)
        } else {
            self.io.flush().map(|_| IoProgress::Pending)
        };
        result.map_err(|error| self.fail(error))
    }
    fn write(&mut self, input: &[u8]) -> Result<IoProgress, Error> {
        if input.is_empty() {
            return Ok(IoProgress::Pending);
        }
        self.live()?;
        let result = if self.info.is_some() {
            self.io.write(input)
        } else {
            self.io.flush().map(|_| IoProgress::Pending)
        };
        result.map_err(|error| self.fail(error))
    }
    fn flush(&mut self) -> Result<FlushProgress, Error> {
        self.live()?;
        self.io.flush().map_err(|error| self.fail(error))
    }
    fn close(&mut self) -> Result<FlushProgress, Error> {
        self.live()?;
        self.io.close().map_err(|error| self.fail(error))
    }
    fn abort(&mut self) {
        self.fail(Error::Invalid);
    }
}
