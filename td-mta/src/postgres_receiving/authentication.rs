//! Per-Email rows for direct-MX sender authentication, written in the
//! delivery's publication transaction.
use super::Error;
use crate::sender_authentication::Results;
use tokio_postgres::Client;

pub(super) async fn write(
    client: &Client,
    account: &[u8],
    email: &[u8],
    results: &Results,
) -> Result<(), Error> {
    let dmarc = &results.dmarc;
    let policy = dmarc.policy;
    let written = client
        .execute(
            "INSERT INTO td_mta.email_authentication(account,email_id,spf_result,spf_domain,
                spf_helo,dkim_complete,dmarc_result,dmarc_author,dmarc_policy_domain,
                dmarc_policy,dmarc_strict_spf,dmarc_strict_dkim,dmarc_aligned)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
            &[
                &account,
                &email,
                &results.spf.as_str(),
                &results.spf_domain.as_deref(),
                &results.spf_helo,
                &!results.dkim.incomplete,
                &dmarc.outcome.as_str(),
                &dmarc.author.as_deref(),
                &dmarc.policy_domain.as_deref(),
                &policy.map(|policy| policy.disposition.as_str()),
                &policy.map(|policy| policy.strict_spf),
                &policy.map(|policy| policy.strict_dkim),
                &dmarc.aligned.map(|aligned| aligned.as_str()),
            ],
        )
        .await
        .map_err(|_| Error::Database)?;
    if written != 1 {
        return Err(Error::Integrity);
    }
    let signatures = &results.dkim.signatures;
    if signatures.is_empty() {
        return Ok(());
    }
    if signatures.len() > crate::dkim::MAX_SIGNATURES {
        return Err(Error::Integrity);
    }
    // At most four rows: fixed arrays, filled in signature order.
    let mut ordinals = [0_i64; crate::dkim::MAX_SIGNATURES];
    let mut outcomes = [""; crate::dkim::MAX_SIGNATURES];
    let mut algorithms = [None; crate::dkim::MAX_SIGNATURES];
    let mut domains = [None; crate::dkim::MAX_SIGNATURES];
    let mut selectors = [None; crate::dkim::MAX_SIGNATURES];
    let mut bits = [None; crate::dkim::MAX_SIGNATURES];
    for (ordinal, row) in signatures.iter().enumerate() {
        if let (Some(o), Some(r), Some(a), Some(d), Some(s), Some(b)) = (
            ordinals.get_mut(ordinal),
            outcomes.get_mut(ordinal),
            algorithms.get_mut(ordinal),
            domains.get_mut(ordinal),
            selectors.get_mut(ordinal),
            bits.get_mut(ordinal),
        ) {
            *o = i64::try_from(ordinal).map_err(|_| Error::Integrity)?;
            *r = row.outcome.as_str();
            *a = row.algorithm.map(|algorithm| algorithm.as_str());
            *d = row.domain.as_deref();
            *s = row.selector.as_deref();
            *b = row.key_bits.map(i32::from);
        } else {
            return Err(Error::Integrity);
        }
    }
    let count = signatures.len();
    let written = client
        .execute(
            "INSERT INTO td_mta.email_dkim_signatures(account,email_id,ordinal,result,
                algorithm,domain,selector,key_bits)
                SELECT $1::bytea,$2::bytea,* FROM unnest($3::bigint[],$4::text[],$5::text[],$6::text[],
                $7::text[],$8::integer[])",
            &[
                &account,
                &email,
                &ordinals.get(..count).ok_or(Error::Integrity)?,
                &outcomes.get(..count).ok_or(Error::Integrity)?,
                &algorithms.get(..count).ok_or(Error::Integrity)?,
                &domains.get(..count).ok_or(Error::Integrity)?,
                &selectors.get(..count).ok_or(Error::Integrity)?,
                &bits.get(..count).ok_or(Error::Integrity)?,
            ],
        )
        .await
        .map_err(|_| Error::Database)?;
    if usize::try_from(written).ok() != Some(count) {
        return Err(Error::Integrity);
    }
    Ok(())
}
