//! Stable source inventory windows; completion still requires change catch-up.
#[path = "replica_cursor.rs"]
mod cursor_wire;

use super::{check_changed, EmailId, Error, Snapshot, Type};
use crate::sync::{DataState, DataType};

pub const MAX_REPLICA_INVENTORY_PAGE: usize = 256;

/// Passive continuation, never authentication or deletion authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicaInventoryCursor {
    pub base: DataState,
    pub upper: EmailId,
    pub after: EmailId,
}

#[derive(Debug)]
pub struct ReplicaInventoryPage {
    pub base: DataState,
    pub observed: DataState,
    pub ids: Vec<EmailId>,
    pub next: Option<ReplicaInventoryCursor>,
}

impl Snapshot<'_> {
    /// Enumerate immutable IDs across separately authenticated snapshots.
    /// Arrivals above the initial upper ID are left to change catch-up.
    /// Changes since `base` must still be applied before claiming completeness.
    pub fn replica_inventory(
        &mut self,
        cursor: Option<ReplicaInventoryCursor>,
        maximum: usize,
    ) -> Result<ReplicaInventoryPage, Error> {
        self.check()?;
        let identity = self.identity;
        let observed = DataState {
            account: identity.account,
            epoch: identity.epoch,
            kind: DataType::Email,
            sequence: identity.committed_sequence,
        };
        let validation = validate(cursor, maximum, observed, identity.history_floor);
        // A client checkpoint refusal is a method error, not a damaged view.
        self.check()?;
        validation?;
        let result = self
            .worker
            .run(|client| {
                Box::pin(async move {
                    let account = identity.account.as_bytes().as_slice();
                    let (base, upper, after) = match cursor {
                        Some(cursor) => (cursor.base, Some(cursor.upper), Some(cursor.after)),
                        None => {
                            let row = client
                                .query_typed_opt(
                                    "SELECT id FROM td_mta.emails WHERE account=$1 ORDER BY id DESC LIMIT 1",
                                    &[(&account, Type::BYTEA)],
                                )
                                .await
                                .map_err(|_| Error::Database)?;
                            let upper = row
                                .map(|row| -> Result<EmailId, Error> {
                                    let bytes: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
                                    Ok(EmailId::from_bytes(bytes.try_into().map_err(|_| Error::Integrity)?))
                                })
                                .transpose()?;
                            (observed, upper, None)
                        }
                    };
                    let mut page = ReplicaInventoryPage {
                        base,
                        observed,
                        ids: Vec::new(),
                        next: None,
                    };
                    let Some(upper) = upper else {
                        return Ok(page);
                    };
                    let upper_bytes = upper.as_bytes().as_slice();
                    let after_bytes = after.as_ref().map(|id| id.as_bytes().as_slice());
                    let limit = (maximum + 1) as i64;
                    let rows = client
                        .query_typed(
                            "SELECT id,changed FROM td_mta.emails WHERE account=$1 AND id<=$2 AND ($3::bytea IS NULL OR id>$3) ORDER BY id LIMIT $4",
                            &[(&account, Type::BYTEA), (&upper_bytes, Type::BYTEA), (&after_bytes, Type::BYTEA), (&limit, Type::INT8)],
                        )
                        .await
                        .map_err(|_| Error::Database)?;
                    if rows.len() > maximum + 1 {
                        return Err(Error::Integrity);
                    }
                    page.ids.try_reserve_exact(maximum).map_err(|_| Error::Capacity)?;
                    let more = rows.len() > maximum;
                    let mut previous = after;
                    for row in rows {
                        let bytes: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
                        let id = EmailId::from_bytes(bytes.try_into().map_err(|_| Error::Integrity)?);
                        check_changed(row.try_get(1).map_err(|_| Error::Integrity)?, identity)?;
                        if id.as_bytes() > upper.as_bytes()
                            || previous.is_some_and(|old| old.as_bytes() >= id.as_bytes())
                        {
                            return Err(Error::Integrity);
                        }
                        previous = Some(id);
                        if page.ids.len() < maximum {
                            page.ids.push(id);
                        }
                    }
                    if more {
                        page.next = Some(ReplicaInventoryCursor {
                            base,
                            upper,
                            after: page.ids.last().copied().ok_or(Error::Integrity)?,
                        });
                    }
                    Ok(page)
                })
            })
            .map_err(Error::Connection)
            .and_then(|result| result);
        self.retain(result)
    }
}

fn validate(
    cursor: Option<ReplicaInventoryCursor>,
    maximum: usize,
    current: DataState,
    floor: crate::format::Sequence,
) -> Result<(), Error> {
    if maximum == 0 {
        return Err(Error::Invalid);
    }
    if maximum > MAX_REPLICA_INVENTORY_PAGE {
        return Err(Error::Capacity);
    }
    if let Some(cursor) = cursor {
        if !cursor.base.is_retained_for(current, floor) {
            return Err(Error::Changes(
                crate::jmap_changes::Error::CannotCalculateChanges,
            ));
        }
        if cursor.after.as_bytes() > cursor.upper.as_bytes() {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        format::Sequence,
        ids::{AccountId, StoreEpoch},
    };

    #[test]
    fn continuation_is_scoped_retained_and_bounded() {
        let current = DataState {
            account: AccountId::from_bytes([1; 16]),
            epoch: StoreEpoch::from_bytes([2; 16]),
            kind: DataType::Email,
            sequence: Sequence::from_u64(20),
        };
        let base = DataState {
            sequence: Sequence::from_u64(10),
            ..current
        };
        let cursor = ReplicaInventoryCursor {
            base,
            after: EmailId::from_bytes([3; 16]),
            upper: EmailId::from_bytes([4; 16]),
        };
        assert_eq!(validate(Some(cursor), 256, current, base.sequence), Ok(()));
        assert_eq!(
            validate(None, 0, current, base.sequence),
            Err(Error::Invalid)
        );
        assert_eq!(
            validate(None, 257, current, base.sequence),
            Err(Error::Capacity)
        );
        let reset = Err(Error::Changes(
            crate::jmap_changes::Error::CannotCalculateChanges,
        ));
        assert_eq!(
            validate(Some(cursor), 1, current, Sequence::from_u64(11)),
            reset
        );
        for base in [
            DataState {
                account: AccountId::from_bytes([9; 16]),
                ..base
            },
            DataState {
                epoch: StoreEpoch::from_bytes([9; 16]),
                ..base
            },
            DataState {
                kind: DataType::Mailbox,
                ..base
            },
            DataState {
                sequence: Sequence::from_u64(21),
                ..base
            },
        ] {
            assert_eq!(
                validate(
                    Some(ReplicaInventoryCursor { base, ..cursor }),
                    1,
                    current,
                    Sequence::from_u64(1)
                ),
                reset
            );
        }
        assert_eq!(
            validate(
                Some(ReplicaInventoryCursor {
                    after: cursor.upper,
                    ..cursor
                }),
                1,
                current,
                base.sequence
            ),
            Ok(())
        );
        assert_eq!(
            validate(
                Some(ReplicaInventoryCursor {
                    after: EmailId::from_bytes([5; 16]),
                    ..cursor
                }),
                1,
                current,
                base.sequence
            ),
            Err(Error::Invalid)
        );
    }
}
