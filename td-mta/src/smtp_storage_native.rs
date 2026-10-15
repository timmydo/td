//! Existing native receiving coordinator adapted to the SMTP worker boundary.
use super::{Begin, Completion, Failure, Job, Storage};
use crate::{
    format::row::ReceiptTls,
    ids::AccountId,
    ports::{Access, Clock, Deadline, Error, Principal},
    store_fs::{
        Delivery, DeliveryAuthorization, DeliveryError, DeliveryGuard, DeliveryPeer,
        DeliveryRequest, IngressSpool, StoreCoordinator, UploadError,
    },
};
use std::{net::IpAddr, sync::Arc};

struct Direct {
    access: Access,
    peer: IpAddr,
    tls: ReceiptTls,
    clock: Arc<dyn Clock>,
}
struct DirectGuard<'a>(&'a Direct);
impl DeliveryGuard for DirectGuard<'_> {
    fn access(&self) -> Access {
        self.0.access
    }
    fn peer(&self) -> DeliveryPeer<'_> {
        DeliveryPeer {
            peer: self.0.peer,
            tls: self.0.tls,
            gateway: None,
        }
    }
}
impl DeliveryAuthorization for Direct {
    type Guard<'a> = DirectGuard<'a>;
    fn authorize(&self, account: AccountId, deadline: Deadline) -> Result<DirectGuard<'_>, Error> {
        if account != self.access.account {
            return Err(Error::Forbidden);
        }
        if deadline.expired(self.clock.sample()?.monotonic) {
            return Err(Error::Deadline);
        }
        Ok(DirectGuard(self))
    }
}

pub struct Native<'s, 'r, 'l, P> {
    pub coordinator: &'s StoreCoordinator<'r, 'l, P>,
    pub spool: &'s IngressSpool<'r>,
    pub crypto: &'s td_crypto::Provider,
}
pub struct NativeJob<'s, 'r, 'l, P>(Delivery<'s, 's, 'r, 'l, td_crypto::Provider, P, Direct>);
fn refusal(error: DeliveryError) -> Failure {
    match error {
        DeliveryError::CoordinationBusy => Failure::Retry,
        DeliveryError::HeaderLimit => Failure::HeaderLimit,
        DeliveryError::Storage(UploadError::Store(error)) => Failure::Refused(error),
        _ => Failure::Refused(Error::Busy),
    }
}
impl<P: Sync> Job for NativeJob<'_, '_, '_, P> {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        self.0.write(bytes).map_err(refusal)
    }
    fn prepare(&mut self, deadline: Deadline) -> Result<(), Failure> {
        self.0.prepare(deadline).map_err(refusal)
    }
    fn commit(&mut self) -> Result<Completion, Failure> {
        let completion = self.0.commit().map_err(refusal)?;
        Ok(Completion {
            outcome: completion.outcome,
            stop: completion.admission_stopped || completion.cleanup_error.is_some(),
        })
    }
}
impl<'r, 'l, P: Sync> Storage for Native<'_, 'r, 'l, P> {
    type Job<'a>
        = NativeJob<'a, 'r, 'l, P>
    where
        Self: 'a;
    fn capacity(&self) -> u64 {
        self.spool.capacity().bytes_each
    }
    fn ready(&self, account: AccountId, deadline: Deadline) -> Result<(), Error> {
        self.coordinator.receiving_ready(account, deadline)
    }
    fn stopped(&self) -> bool {
        self.coordinator.admission_stopped()
    }
    fn begin(
        &self,
        input: Begin<'_, '_>,
        random: &mut td_crypto::SystemEntropy,
    ) -> Result<Self::Job<'_>, Failure> {
        let authorization = Direct {
            access: Access {
                account: input.account,
                principal: Principal::Smtp,
                config_generation: input.config_generation,
            },
            peer: input.peer,
            tls: input.tls,
            clock: input.clock,
        };
        self.coordinator
            .reserve_delivery(
                self.crypto,
                random,
                self.spool,
                authorization,
                input.session,
                DeliveryRequest {
                    header_bytes: input.header_bytes,
                    deadline: input.deadline,
                },
            )
            .map(NativeJob)
            .map_err(|error| match error {
                DeliveryError::Storage(UploadError::Store(Error::Busy))
                | DeliveryError::Storage(UploadError::Ledger(
                    crate::admission::logical::Error::Slot(crate::ownership::Error::Contended),
                )) => Failure::Retry,
                other => refusal(other),
            })
    }
}
