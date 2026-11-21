//! DKIM and DMARC share one worker, DNS meter and borrow of the admitted DATA.
use super::{Cell, Control, Owner, Phase, Receiving, Slot};
use crate::{
    clock::RuntimeClock,
    dkim, dmarc,
    ports::{Clock, Deadline},
    smtp_storage::{Job, Storage},
    spf::{Dns, DnsError, Query, Record},
    spf_dns::Resolver,
};
use std::{
    net::SocketAddr,
    sync::atomic::{AtomicBool, Ordering},
    thread,
};

pub(crate) const STACK_BYTES: usize = 1024 * 1024;

pub(super) struct MessageAuthentication {
    pub(super) dkim: dkim::Report,
    pub(super) dmarc: dmarc::PendingReport,
}

impl MessageAuthentication {
    pub(super) fn unavailable() -> Self {
        Self {
            dkim: dkim::Report::unavailable(),
            dmarc: dmarc::PendingReport::complete(dmarc::Report::unavailable()),
        }
    }
}

struct ObservationDns<'a>(Option<Resolver<'a>>);
impl Dns for ObservationDns<'_> {
    fn lookup(&mut self, name: &str, kind: Query) -> Result<Option<Vec<Record>>, DnsError> {
        self.0
            .as_mut()
            .ok_or(DnsError::Unavailable)?
            .lookup(name, kind)
    }
}

fn evaluate_message(
    raw: &[u8],
    header_limit: usize,
    envelope: &crate::smtp_session::Envelope,
    spf: Option<crate::spf::Outcome>,
    dns: &mut impl Dns,
    now: u64,
    current: &dyn Fn() -> bool,
) -> MessageAuthentication {
    let dkim = dkim::evaluate(raw, header_limit, dns, now, current);
    let observe_dmarc = || {
        let author = match dmarc::author::domain(raw, header_limit, current) {
            Ok(author) => author,
            Err(dmarc::author::Error::Invalid) => {
                return dmarc::PendingReport::complete(dmarc::Report::invalid_author());
            }
            Err(dmarc::author::Error::Unavailable) => {
                return dmarc::PendingReport::complete(dmarc::Report::unavailable());
            }
        };
        let domain = crate::spf::mailfrom_identity(&envelope.ehlo, &envelope.reverse_path)
            .map(|(_, domain)| domain);
        dmarc::evaluate(&author, domain, spf, &dkim, dns, current)
    };
    // A failed DMARC worker operation cannot erase completed signature results.
    let dmarc = std::panic::catch_unwind(std::panic::AssertUnwindSafe(observe_dmarc))
        .unwrap_or_else(|_| dmarc::PendingReport::complete(dmarc::Report::unavailable()));
    MessageAuthentication { dkim, dmarc }
}

impl<'s, 'cfg, B: Storage> Receiving<'s, 'cfg, B> {
    pub(super) fn verify_message(
        &self,
        slots: &[Cell<Slot<'s, 'cfg, '_, B>>],
        finished: &AtomicBool,
        control: &Control,
        servers: &[SocketAddr],
    ) {
        let clock = RuntimeClock::new();
        let mut packet =
            td_alloc::filled(crate::dns_transport::PACKET_BYTES, 0u8).unwrap_or_default();
        let mut entropy = td_crypto::SystemEntropy::try_new().ok();
        while !finished.load(Ordering::Acquire) {
            let mut worked = false;
            let mut contended = false;
            for cell in slots {
                if !cell.owned(Owner::Dkim) {
                    continue;
                }
                let mut slot = match cell.slot.try_lock() {
                    Ok(slot) => slot,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        contended = true;
                        continue;
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        control.fail();
                        return;
                    }
                };
                if !cell.owned(Owner::Dkim) {
                    continue;
                }
                let Phase::AwaitAuthentication { until } = slot.phase else {
                    continue;
                };
                if slot.authentication.is_some() {
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
                    let envelope = slot
                        .connection
                        .as_ref()?
                        .network()
                        .ok()?
                        .session()
                        .envelope()?;
                    Some(evaluate_message(
                        raw,
                        self.resources.limits().header_bytes,
                        envelope,
                        slot.spf_result,
                        &mut ObservationDns(dns),
                        u64::try_from(time.utc_ms.div_euclid(1000)).ok()?,
                        &current,
                    ))
                };
                // A verifier failure must release the transaction for ordinary custody.
                let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(observe))
                    .ok()
                    .flatten()
                    .unwrap_or_else(MessageAuthentication::unavailable);
                slot.authentication = Some(report);
                cell.assign(Owner::Network);
                drop(slot);
                worked = true;
                control.smtp_wake.notify();
            }
            if !worked && !finished.load(Ordering::Acquire) {
                if contended {
                    thread::park_timeout(super::SCAN);
                } else {
                    thread::park();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spf;

    struct Policy;
    impl Dns for Policy {
        fn lookup(&mut self, name: &str, kind: Query) -> Result<Option<Vec<Record>>, DnsError> {
            assert_eq!(kind, Query::Txt);
            assert_eq!(name, "_dmarc.author.example");
            Ok(Some(vec![Record::Text(
                b"v=DMARC1; p=reject; aspf=s; adkim=s".to_vec(),
            )]))
        }
    }

    #[test]
    fn final_spf_uses_only_the_session_mail_from_identity() {
        for (reverse, spf, expected) in [
            ("", spf::Outcome::Pass, "pass"),
            ("sender@other.example", spf::Outcome::Pass, "fail"),
            ("sender@author.example", spf::Outcome::Fail, "fail"),
        ] {
            let envelope = crate::smtp_session::Envelope {
                ehlo: "author.example".into(),
                reverse_path: reverse.into(),
                recipients: Vec::new(),
                eight_bit: false,
                declared_bytes: None,
            };
            let report = evaluate_message(
                b"From: Sender <owner@author.example>\r\n\r\nbody\r\n",
                4096,
                &envelope,
                None,
                &mut Policy,
                0,
                &|| true,
            );
            assert!(report.dkim.signatures.is_empty() && !report.dkim.incomplete);
            assert_eq!(report.dmarc.finish(spf).outcome.as_str(), expected);
        }
    }
}
