//! Live phase validation; returned bounds are passive observations, not SMTP grants.
use super::{capacity, number, receipt, valid_reply, Change, Error, Observation};
use crate::{
    ports::Clock,
    postgres_probe::{self, Request},
    postgres_route_owners as owners,
    queue_clock::Guard,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use td_crypto::TrustStore;

/// Validate the exact still-active AcceptancePossible subset and its durable
/// capacity under fresh policy, ownership and queue-clock checks. All database
/// locks end before return. Revalidation cannot revoke already buffered bytes.
pub fn check_current_possible(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Change,
    clock: Arc<dyn Clock>,
    guard: Arc<Mutex<Guard>>,
) -> Result<Instant, Error> {
    if value.inner.phase != 3 {
        return Err(Error::Integrity);
    }
    let value = value.inner.clone();
    let observation = Observation { clock, guard };
    let limit = postgres_probe::with_batch_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            if !owners::begin(client, &value.claim, true, false).await? {
                return Err(Error::Policy);
            }
            let timestamp = observation.check(client, &value).await?;
            crate::postgres_route_health::admit_locked(client, &value.claim, timestamp).await.map_err(Error::Health)?;
            let published = receipt(client, &value).await?.ok_or(Error::Missing)?;
            let binding = value.claim.binding();
            let account_id = binding.account();
            let account = account_id.as_bytes().as_slice();
            let attempt = value.attempt.as_bytes().as_slice();
            let rows = client.query("SELECT a.submission_id,s.expires_at,s.completed_at,s.changed,n.sequence,n.floor,a.ordinals FROM td_mta.submission_attempts a JOIN td_mta.submissions s ON s.account=a.account AND s.id=a.submission_id JOIN td_mta.accounts n ON n.id=a.account WHERE a.account=$1 AND a.attempt_id=$2 AND a.active AND a.route_name=$3 AND a.destination=$4 AND a.generation=$5 AND a.owner_boot=$6 AND a.owner_operation=$7", &[&account,&attempt,&binding.route().name(),&binding.route().destination().as_slice(),&value.generation.to_be_bytes().as_slice(),&value.claim.boot().as_bytes().as_slice(),&value.claim.operation().as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
            let row = rows.first().ok_or(Error::Missing)?;
            let submission: &[u8] = row.try_get(0).map_err(|_|Error::Integrity)?;
            let expires: i64 = row.try_get(1).map_err(|_|Error::Integrity)?;
            let completed: Option<i64> = row.try_get(2).map_err(|_|Error::Integrity)?;
            let sequence = number(row.try_get(4).map_err(|_|Error::Integrity)?)?;
            let encoded:&[u8]=row.try_get(6).map_err(|_|Error::Integrity)?;
            if encoded.is_empty() || encoded.len()>400 || !encoded.len().is_multiple_of(4) {
                return Err(Error::Integrity);
            }
            if submission.len()!=16 || number(row.try_get(3).map_err(|_|Error::Integrity)?)?>sequence
                || number(row.try_get(5).map_err(|_|Error::Integrity)?)?>sequence
                || published.new_sequence>sequence {
                return Err(Error::Integrity);
            }
            if completed.is_some() || timestamp>=expires {
                return Err(Error::ExpiryHold);
            }
            crate::postgres_final_capacity::check(client, account, attempt, &value.ordinals).await.map_err(capacity)?;
            let rows=client.query("SELECT ordinal,phase,rcpt_reply,data_reply,reason,changed,next_attempt_at FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND attempt_id=$3 AND state=2 ORDER BY ordinal LIMIT 101", &[&account,&submission,&attempt]).await.map_err(|_|Error::Database)?;
            if rows.len()>100 {return Err(Error::Integrity);}
            let mut selected=value.ordinals.iter();
            for row in &rows {
                let found:i64=row.try_get(0).map_err(|_|Error::Integrity)?;
                let phase:i64=row.try_get(1).map_err(|_|Error::Integrity)?;
                let rcpt:Option<&str>=row.try_get(2).map_err(|_|Error::Integrity)?;
                let data:Option<&str>=row.try_get(3).map_err(|_|Error::Integrity)?;
                let reason:i64=row.try_get(4).map_err(|_|Error::Integrity)?;
                let next:Option<i64>=row.try_get(6).map_err(|_|Error::Integrity)?;
                let changed=number(row.try_get(5).map_err(|_|Error::Integrity)?)?;
                if !(0..1000).contains(&found) || !encoded.as_chunks::<4>().0.iter().any(|n|i64::from(u32::from_be_bytes(*n))==found)
                    || reason!=0 || next.is_some() || changed>sequence {return Err(Error::Integrity);}
                if phase==1 && value.ordinals.binary_search(&found).is_err() {continue;}
                if phase!=3 || selected.next()!=Some(&found) {return Err(Error::Conflict);}
                if rcpt.is_none_or(|s|!valid_reply(s,false)) || data.is_some() || changed!=published.new_sequence {
                    return Err(Error::Integrity);
                }
            }
            if selected.next().is_some() {return Err(Error::Conflict);}
            let started = Instant::now();
            let (finished, owner_expiry) = owners::check_locked_expiry_observed(client, &value.claim, value.generation, |utc_ms| {
                crate::queue_clock::admit_database(observation.clock.as_ref(), &observation.guard, utc_ms, false).map_err(Error::Clock)
            }).await?;
            if finished<timestamp {return Err(Error::ExpiryHold);}
            local_limit(started, deadline, finished, owner_expiry.min(expires))
        })
    }).map_err(Error::Connection)??;
    if Instant::now() >= limit {
        return Err(Error::ExpiryHold);
    }
    Ok(limit)
}
fn local_limit(
    started: Instant,
    deadline: Instant,
    observed: i64,
    expires: i64,
) -> Result<Instant, Error> {
    // Database UTC rounds to milliseconds; discard the fractional boundary.
    let remaining = expires
        .checked_sub(observed)
        .and_then(|n| n.checked_sub(1))
        .filter(|n| *n > 0)
        .ok_or(Error::ExpiryHold)?;
    let millis = u64::try_from(remaining).map_err(|_| Error::Integrity)?;
    let limit = started
        .checked_add(Duration::from_millis(millis))
        .ok_or(Error::Capacity)?
        .min(deadline);
    if Instant::now() >= limit {
        return Err(Error::ExpiryHold);
    }
    Ok(limit)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn local_bound_keeps_query_start_deadline_and_rounding_margin() {
        let start = Instant::now();
        let cap = start + Duration::from_secs(60);
        assert_eq!(
            local_limit(start, cap, 10_000, 20_000),
            Ok(start + Duration::from_millis(9999))
        );
        let short = start + Duration::from_secs(2);
        assert_eq!(local_limit(start, short, 10_000, 20_000), Ok(short));
        for (observed, expires) in [(10, 10), (10, 9), (10, 11), (i64::MIN, i64::MAX)] {
            assert_eq!(
                local_limit(start, cap, observed, expires),
                Err(Error::ExpiryHold)
            );
        }
        let old = start.checked_sub(Duration::from_secs(20)).unwrap();
        assert_eq!(
            local_limit(old, cap, 10_000, 20_000),
            Err(Error::ExpiryHold)
        );
        assert_eq!(
            local_limit(start, old, 10_000, 20_000),
            Err(Error::ExpiryHold)
        );
    }
}
