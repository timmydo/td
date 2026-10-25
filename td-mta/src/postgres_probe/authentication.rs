//! Fresh device observations through one finite qualified read session.
use super::{Error, ReadIsolation, ReadSession, Request};
use crate::{
    device_credentials::Presented, http_auth::Principal, ports, postgres_runtime_profile::Binding,
    postgres_snapshot,
};
use std::time::Instant;
use td_crypto::TrustStore;

pub(crate) struct AuthenticationSession(ReadSession);
impl AuthenticationSession {
    pub(crate) fn open(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
    ) -> Result<Self, Error> {
        ReadSession::open(request, trust, deadline).map(Self)
    }
    pub(crate) fn reusable(&self, deadline: Instant) -> bool {
        self.0.reusable(deadline)
    }
    pub(crate) fn check_idle(&mut self, deadline: Instant) -> Result<(), ports::Error> {
        self.0.check_idle(deadline).map_err(connection)
    }
    pub(crate) fn needs_retirement(&self, now: Instant) -> bool {
        self.0.needs_retirement(now)
    }
    pub(crate) fn retire(self, deadline: Instant) {
        self.0.retire(deadline);
    }
    pub(crate) fn authenticate(
        &mut self,
        binding: &Binding,
        presented: Presented,
        deadline: Instant,
    ) -> Result<Principal, ports::Error> {
        let device = presented.id();
        let binding = binding.clone();
        self.0
            .read(
                binding.epoch(),
                ReadIsolation::RepeatableReadOnly,
                deadline,
                move |client| {
                    Box::pin(async move {
                        match postgres_snapshot::observe(client, &binding, presented).await {
                            Ok(_) => Ok(Some(Principal {
                                account: binding.account(),
                                device,
                            })),
                            Err(postgres_snapshot::Error::Unauthenticated) => Ok(None),
                            Err(_) => Err(ports::Error::Invalid),
                        }
                    })
                },
            )
            .map_err(connection)??
            .ok_or(ports::Error::Forbidden)
    }
}
fn connection(error: Error) -> ports::Error {
    match error {
        Error::Deadline => ports::Error::Deadline,
        _ => ports::Error::Invalid,
    }
}
