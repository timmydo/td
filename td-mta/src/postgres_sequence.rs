//! One next-sequence check inside the caller's fenced account transaction.
use tokio_postgres::Client;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Capacity,
    Integrity,
    Database,
}

/// Caller already holds this account's row lock in the documented lock order.
/// Recheck the observed sequence under that lock; publication and COMMIT stay
/// with the caller. Preserve one future increment for every final-result claim.
pub(crate) async fn next(client: &Client, account: &[u8], observed: u64) -> Result<u64, Error> {
    if account.len() != 16 {
        return Err(Error::Integrity);
    }
    let row = client
        .query_one(
            "SELECT sequence,floor,(SELECT count(*) FROM (SELECT 1 FROM td_mta.attempt_final_reservations WHERE account=$1 LIMIT 1025) claims) FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
            &[&account],
        )
        .await
        .map_err(|_| Error::Database)?;
    let claims: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
    if !(0..=1024).contains(&claims) {
        return Err(Error::Integrity);
    }
    let claims = u64::try_from(claims).map_err(|_| Error::Integrity)?;
    advance(
        row.try_get(0).map_err(|_| Error::Integrity)?,
        row.try_get(1).map_err(|_| Error::Integrity)?,
        observed,
        claims,
    )
}
fn advance(sequence: &[u8], floor: &[u8], observed: u64, claims: u64) -> Result<u64, Error> {
    let sequence = u64::from_be_bytes(sequence.try_into().map_err(|_| Error::Integrity)?);
    let floor = u64::from_be_bytes(floor.try_into().map_err(|_| Error::Integrity)?);
    if sequence != observed || floor > sequence {
        return Err(Error::Integrity);
    }
    sequence
        .checked_add(claims)
        .and_then(|n| n.checked_add(1))
        .ok_or(Error::Capacity)?;
    sequence.checked_add(1).ok_or(Error::Capacity)
}
#[cfg(test)]
mod tests {
    use super::{advance, Error};
    #[test]
    fn exact_unsigned_sequence_and_floor_boundaries() {
        for sequence in [0, 1, i64::MAX as u64, u64::MAX - 1] {
            assert_eq!(
                advance(
                    &sequence.to_be_bytes(),
                    &sequence.to_be_bytes(),
                    sequence,
                    0
                ),
                Ok(sequence + 1)
            );
        }
        assert_eq!(
            advance(&u64::MAX.to_be_bytes(), &0u64.to_be_bytes(), u64::MAX, 0),
            Err(Error::Capacity)
        );
        assert_eq!(
            advance(&1u64.to_be_bytes(), &0u64.to_be_bytes(), 0, 0),
            Err(Error::Integrity)
        );
        assert_eq!(
            advance(&1u64.to_be_bytes(), &2u64.to_be_bytes(), 1, 0),
            Err(Error::Integrity)
        );
        assert_eq!(
            advance(
                &(u64::MAX - 1).to_be_bytes(),
                &0u64.to_be_bytes(),
                u64::MAX - 1,
                1
            ),
            Err(Error::Capacity)
        );
        assert_eq!(
            advance(
                &(u64::MAX - 2).to_be_bytes(),
                &0u64.to_be_bytes(),
                u64::MAX - 2,
                1
            ),
            Ok(u64::MAX - 1)
        );
        assert_eq!(advance(&[0; 7], &[0; 8], 0, 0), Err(Error::Integrity));
        assert_eq!(advance(&[0; 8], &[0; 9], 0, 0), Err(Error::Integrity));
    }
}
