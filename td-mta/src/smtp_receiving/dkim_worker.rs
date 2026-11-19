//! One DNS and signature worker borrows a completed transaction's admitted DATA.
use super::{Control, Phase, Receiving, Slot};
use crate::{
    clock::RuntimeClock,
    dkim::Report,
    ports::{Clock, Deadline},
    smtp_storage::{Job, Storage},
    spf::{Dns, DnsError, Query, Record},
    spf_dns::Resolver,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, TryLockError,
    },
    thread,
};

pub(crate) const STACK_BYTES: usize = 1024 * 1024;

struct KeyDns<'a>(Option<Resolver<'a>>);
impl Dns for KeyDns<'_> {
    fn lookup(&mut self, name: &str, kind: Query) -> Result<Option<Vec<Record>>, DnsError> {
        self.0
            .as_mut()
            .ok_or(DnsError::Unavailable)?
            .lookup(name, kind)
    }
}

impl<'s, 'cfg, B: Storage> Receiving<'s, 'cfg, B> {
    pub(super) fn verify_dkim(
        &self,
        slots: &[Mutex<Slot<'s, 'cfg, '_, B>>],
        finished: &AtomicBool,
        main: &thread::Thread,
        control: &Control,
        servers: &[SocketAddr],
    ) {
        let clock = RuntimeClock::new();
        let mut packet =
            td_alloc::filled(crate::dns_transport::PACKET_BYTES, 0u8).unwrap_or_default();
        let mut entropy = td_crypto::SystemEntropy::try_new().ok();
        while !finished.load(Ordering::Acquire) {
            let mut contended = false;
            for cell in slots {
                let mut slot = match cell.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::WouldBlock) => {
                        contended = true;
                        continue;
                    }
                    Err(TryLockError::Poisoned(_)) => continue,
                };
                let Phase::AwaitAuthentication { until } = slot.phase else {
                    continue;
                };
                if slot.dkim_report.is_some() {
                    continue;
                }
                let observe = || {
                    let raw = slot.delivery.as_ref()?.dkim_input()?;
                    // Sample the worker's DNS clock first so the conversion cannot
                    // extend the absolute deadline established at the final dot.
                    let origin = clock.monotonic().ok()?;
                    let time = self.clock.sample().ok()?;
                    let remaining = until.0.checked_sub(time.monotonic.0)?;
                    let deadline = Deadline::after(origin, remaining).ok()?;
                    let current = || {
                        !finished.load(Ordering::Acquire)
                            && !control.stop_requested()
                            && clock.monotonic().is_ok_and(|now| !deadline.expired(now))
                    };
                    if !current() {
                        return None;
                    }
                    let dns = entropy
                        .as_mut()
                        .and_then(|random| {
                            Resolver::new(servers, &clock, deadline, random, &mut packet).ok()
                        })
                        .map(|resolver| resolver.while_current(&current));
                    Some(crate::dkim::evaluate(
                        raw,
                        self.resources.limits().header_bytes,
                        &mut KeyDns(dns),
                        u64::try_from(time.utc_ms.div_euclid(1000)).ok()?,
                        &current,
                    ))
                };
                // A verifier failure must release the transaction for ordinary custody.
                let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(observe))
                    .ok()
                    .flatten()
                    .unwrap_or_else(Report::unavailable);
                slot.dkim_report = Some(report);
                drop(slot);
                main.unpark();
            }
            if !finished.load(Ordering::Acquire) {
                if contended {
                    thread::park_timeout(super::SCAN);
                } else {
                    thread::park();
                }
            }
        }
    }
}
