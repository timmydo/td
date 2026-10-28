//! Immutable next-page staging; a completed old page is required at commit.
use crate::{
    ids::{AccountId, EmailId},
    postgres_read::ReplicaInventoryCursor,
    postgres_replica_source::Source,
    replica_pending::{Pending, Ticket},
    replica_source::{Changes, Inventory},
    sync::{DataState, DataType},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Source,
    Capacity,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Anchor {
    Inventory(ReplicaInventoryCursor),
    Changes(DataState),
}
pub struct Transition {
    pub(crate) source: Source,
    pub(crate) account: AccountId,
    pub(crate) previous: Ticket,
    pub(crate) anchor: Anchor,
    pub(crate) pending: Pending,
}
impl Transition {
    /// A next inventory page must retain the exact requested continuation.
    pub fn inventory(
        source: &Source,
        account: AccountId,
        previous: Ticket,
        page: &Inventory,
    ) -> Result<Self, Error> {
        if page.ids.len() > crate::postgres_read::MAX_REPLICA_INVENTORY_PAGE {
            return Err(Error::Capacity);
        }
        let cursor = page.previous.ok_or(Error::Invalid)?;
        if cursor.base != page.base
            || page
                .ids
                .iter()
                .any(|id| *id <= cursor.after || *id > cursor.upper)
            || page.next.is_some_and(|next| next.upper != cursor.upper)
        {
            return Err(Error::Invalid);
        }
        let pending = Pending::inventory(next(previous)?, page).map_err(|_| Error::Invalid)?;
        Self::new(
            source,
            account,
            previous,
            Anchor::Inventory(cursor),
            pending,
        )
    }
    pub fn changes(
        source: &Source,
        account: AccountId,
        previous: Ticket,
        page: &Changes<EmailId>,
    ) -> Result<Self, Error> {
        let pending = Pending::changes(next(previous)?, page).map_err(|_| Error::Invalid)?;
        Self::new(
            source,
            account,
            previous,
            Anchor::Changes(page.old),
            pending,
        )
    }
    fn new(
        source: &Source,
        account: AccountId,
        previous: Ticket,
        anchor: Anchor,
        pending: Pending,
    ) -> Result<Self, Error> {
        let observed = pending.observed();
        if observed.account != account
            || observed.epoch != source.epoch()
            || observed.kind != DataType::Email
        {
            return Err(Error::Source);
        }
        Ok(Self {
            source: Source::new(source.origin(), source.epoch()).map_err(|_| Error::Source)?,
            account,
            previous,
            anchor,
            pending,
        })
    }
    pub fn ticket(&self) -> Ticket {
        self.pending.ticket()
    }
}
fn next(previous: Ticket) -> Result<Ticket, Error> {
    if previous.generation == 0 || previous.page == 0 {
        return Err(Error::Invalid);
    }
    Ok(Ticket {
        generation: previous.generation,
        page: previous.page.checked_add(1).ok_or(Error::Capacity)?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{format::Sequence, ids::StoreEpoch};
    fn state(number: u64) -> DataState {
        DataState {
            epoch: StoreEpoch::from_bytes([1; 16]),
            account: AccountId::from_bytes([2; 16]),
            kind: DataType::Email,
            sequence: Sequence::from_u64(number),
        }
    }
    fn source() -> Source {
        Source::new("https://mail.example.test", state(1).epoch).unwrap()
    }
    fn ticket() -> Ticket {
        Ticket {
            generation: 1,
            page: 1,
        }
    }
    fn page() -> Inventory {
        Inventory {
            previous: Some(ReplicaInventoryCursor {
                base: state(3),
                upper: EmailId::from_bytes([9; 16]),
                after: EmailId::from_bytes([1; 16]),
            }),
            base: state(3),
            observed: state(5),
            ids: vec![EmailId::from_bytes([2; 16])],
            next: None,
        }
    }
    #[test]
    fn inventory_requires_the_exact_scoped_continuation_and_bounded_ids() {
        let source = source();
        let account = state(1).account;
        let good = page();
        assert_eq!(
            Transition::inventory(&source, account, ticket(), &good)
                .unwrap()
                .ticket(),
            Ticket {
                generation: 1,
                page: 2
            }
        );
        let mut oversized = page();
        oversized.ids = vec![EmailId::from_bytes([2; 16]); 257];
        assert_eq!(
            Transition::inventory(&source, account, ticket(), &oversized).err(),
            Some(Error::Capacity)
        );
        let mut bad = page();
        bad.previous = None;
        assert!(Transition::inventory(&source, account, ticket(), &bad).is_err());
        let mut bad = page();
        bad.ids = vec![EmailId::from_bytes([1; 16])];
        assert!(Transition::inventory(&source, account, ticket(), &bad).is_err());
        let mut bad = page();
        bad.ids = vec![EmailId::from_bytes([10; 16])];
        assert!(Transition::inventory(&source, account, ticket(), &bad).is_err());
        let mut bad = page();
        bad.next = Some(ReplicaInventoryCursor {
            base: state(3),
            upper: EmailId::from_bytes([8; 16]),
            after: EmailId::from_bytes([2; 16]),
        });
        assert!(Transition::inventory(&source, account, ticket(), &bad).is_err());
        assert!(
            Transition::inventory(&source, AccountId::from_bytes([3; 16]), ticket(), &good)
                .is_err()
        );
        for prior in [
            Ticket {
                generation: 0,
                page: 1,
            },
            Ticket {
                generation: 1,
                page: 0,
            },
            Ticket {
                generation: 1,
                page: u64::MAX,
            },
        ] {
            assert!(Transition::inventory(&source, account, prior, &good).is_err());
        }
    }
    #[test]
    fn changes_retain_replay_anchor_and_do_not_grant_completion() {
        let source = source();
        let account = state(1).account;
        let page = Changes {
            old: state(3),
            new: state(5),
            more: false,
            created: vec![],
            updated: vec![EmailId::from_bytes([1; 16])],
            destroyed: vec![],
        };
        let transition = Transition::changes(&source, account, ticket(), &page).unwrap();
        assert_eq!(transition.anchor, Anchor::Changes(state(3)));
        assert!(!transition.pending.complete());
        let bad = Changes {
            old: state(5),
            ..page
        };
        assert!(Transition::changes(&source, account, ticket(), &bad).is_err());
    }
}
