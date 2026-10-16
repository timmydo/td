//! Durable logical final-result capacity under the caller's account row lock.
use tokio_postgres::Client;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Capacity,
    Integrity,
    Database,
}
#[derive(Clone, Copy)]
pub(crate) enum Receipt {
    Accepted,
    Refused,
    Recovery,
    Expiry,
}
/// Every caller holds the account row lock before inspecting or changing claims.
pub(crate) async fn reserved(client: &Client, account: &[u8]) -> Result<u64, Error> {
    let count: i64 = client.query_one(
        "SELECT count(*) FROM (SELECT 1 FROM td_mta.attempt_final_reservations WHERE account=$1 LIMIT 1025) claims", &[&account]
    ).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
    if !(0..=1024).contains(&count) {
        return Err(Error::Integrity);
    }
    u64::try_from(count).map_err(|_| Error::Integrity)
}
fn available(used: i64, claims: u64) -> Result<(), Error> {
    if !(0..=1024).contains(&used) || claims > 1024 {
        return Err(Error::Integrity);
    }
    if u64::try_from(used).map_err(|_| Error::Integrity)? + claims >= 1024 {
        return Err(Error::Capacity);
    }
    Ok(())
}
pub(crate) async fn admit_receipt(
    client: &Client,
    account: &[u8],
    kind: Receipt,
) -> Result<(), Error> {
    let query = match kind {
        Receipt::Accepted => "SELECT count(*) FROM (SELECT 1 FROM td_mta.attempt_acceptance_receipts WHERE account=$1 LIMIT 1024) receipts",
        Receipt::Refused => "SELECT count(*) FROM (SELECT 1 FROM td_mta.attempt_refusal_receipts WHERE account=$1 LIMIT 1024) receipts",
        Receipt::Recovery => "SELECT count(*) FROM (SELECT 1 FROM td_mta.attempt_recovery_receipts WHERE account=$1 LIMIT 1024) receipts",
        Receipt::Expiry => "SELECT count(*) FROM (SELECT 1 FROM td_mta.submission_expiry_receipts WHERE account=$1 LIMIT 1024) receipts",
    };
    let used = client
        .query_one(query, &[&account])
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Integrity)?;
    available(used, reserved(client, account).await?)
}
fn encode(ordinals: &[i64]) -> Result<Vec<u8>, Error> {
    if ordinals.is_empty() || ordinals.len() > 100 {
        return Err(Error::Integrity);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(ordinals.len() * 4)
        .map_err(|_| Error::Capacity)?;
    let mut previous = None;
    for ordinal in ordinals {
        if !(0..1000).contains(ordinal) || previous.is_some_and(|p| p >= *ordinal) {
            return Err(Error::Integrity);
        }
        bytes.extend_from_slice(
            &u32::try_from(*ordinal)
                .map_err(|_| Error::Integrity)?
                .to_be_bytes(),
        );
        previous = Some(*ordinal);
    }
    Ok(bytes)
}
pub(crate) async fn reserve(
    client: &Client,
    account: &[u8],
    attempt: &[u8],
    ordinals: &[i64],
) -> Result<(), Error> {
    let bytes = encode(ordinals)?;
    for kind in [
        Receipt::Accepted,
        Receipt::Refused,
        Receipt::Recovery,
        Receipt::Expiry,
    ] {
        admit_receipt(client, account, kind).await?;
    }
    if client
        .execute(
            "INSERT INTO td_mta.attempt_final_reservations VALUES($1,$2,$3)",
            &[&account, &attempt, &bytes],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    Ok(())
}
/// Consume only the entire still-AcceptancePossible subset in the same final
/// transaction. Empty means this attempt had no possible remote acceptance.
pub(crate) async fn consume(
    client: &Client,
    account: &[u8],
    attempt: &[u8],
    ordinals: &[i64],
) -> Result<(), Error> {
    let rows = client.query("SELECT ordinals FROM td_mta.attempt_final_reservations WHERE account=$1 AND attempt_id=$2 FOR UPDATE", &[&account,&attempt]).await.map_err(|_| Error::Database)?;
    let Some(row) = rows.first() else {
        return if ordinals.is_empty() {
            Ok(())
        } else {
            Err(Error::Integrity)
        };
    };
    let stored: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if stored != encode(ordinals)? {
        return Err(Error::Integrity);
    }
    if client
        .execute(
            "DELETE FROM td_mta.attempt_final_reservations WHERE account=$1 AND attempt_id=$2",
            &[&account, &attempt],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    Ok(())
}
pub(crate) struct Subset {
    pub attempt: Vec<u8>,
    pub ordinals: Vec<i64>,
}
/// Expiry must finish one reserved subset in a single bounded publication.
pub(crate) async fn expiry_subset(
    client: &Client,
    account: &[u8],
    submission: &[u8],
) -> Result<Option<Subset>, Error> {
    let rows = client.query("SELECT c.attempt_id,c.ordinals,a.active FROM td_mta.attempt_final_reservations c JOIN td_mta.submission_attempts a USING(account,attempt_id) WHERE c.account=$1 AND a.submission_id=$2 ORDER BY c.attempt_id LIMIT 1 FOR UPDATE OF c,a", &[&account,&submission]).await.map_err(|_| Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let attempt: Vec<u8> = row.try_get(0).map_err(|_| Error::Integrity)?;
    let bytes: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let active: bool = row.try_get(2).map_err(|_| Error::Integrity)?;
    if attempt.len() != 16
        || !active
        || bytes.is_empty()
        || bytes.len() > 400
        || !bytes.len().is_multiple_of(4)
    {
        return Err(Error::Integrity);
    }
    let mut ordinals = Vec::new();
    ordinals
        .try_reserve_exact(bytes.len() / 4)
        .map_err(|_| Error::Capacity)?;
    for chunk in bytes.as_chunks::<4>().0 {
        ordinals.push(i64::from(u32::from_be_bytes(*chunk)));
    }
    if encode(&ordinals)? != bytes {
        return Err(Error::Integrity);
    }
    Ok(Some(Subset { attempt, ordinals }))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claims_share_each_receipt_ceiling_and_subsets_are_exact() {
        assert_eq!(available(1023, 0), Ok(()));
        assert_eq!(available(1022, 1), Ok(()));
        for (used, claims) in [(1024, 0), (1023, 1), (0, 1024)] {
            assert_eq!(available(used, claims), Err(Error::Capacity));
        }
        assert_eq!(available(-1, 0), Err(Error::Integrity));
        assert_eq!(available(0, 1025), Err(Error::Integrity));
        assert_eq!(encode(&[0, 999]), Ok(vec![0, 0, 0, 0, 0, 0, 3, 231]));
        for values in [
            vec![],
            vec![0, 0],
            vec![2, 1],
            vec![-1],
            vec![1000],
            (0..101).collect(),
        ] {
            assert_eq!(encode(&values), Err(Error::Integrity));
        }
    }
}
