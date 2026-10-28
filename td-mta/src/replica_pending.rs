//! Bounded page effects; completion is persisted with each local publication.
use crate::{
    ids::EmailId,
    postgres_read::{ReplicaInventoryCursor, MAX_REPLICA_INVENTORY_PAGE},
    replica_source::{Changes, Inventory},
    sync::{DataState, DataType},
};

pub const MAX_IDS: usize = crate::jmap_changes::MAX_CHANGES;
pub const DONE_BYTES: usize = MAX_IDS.div_ceil(8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Capacity,
    Scope,
    Stale,
    Missing,
}

/// Passive operation identity. The publisher checks it against its locked row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ticket {
    pub generation: u64,
    pub page: u64,
}
impl Ticket {
    fn validate(self) -> Result<(), Error> {
        if self.generation == 0 || self.page == 0 {
            Err(Error::Invalid)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Advance {
    Inventory(Option<ReplicaInventoryCursor>),
    Changes { state: DataState, more: bool },
}

/// A page owns its exact IDs across restart, including the first inventory page.
/// It confers no authority to advance a durable checkpoint outside its transaction.
pub struct Pending {
    ticket: Ticket,
    observed: DataState,
    ids: Vec<EmailId>,
    done: [u8; DONE_BYTES],
    advance: Advance,
}
impl Pending {
    pub fn inventory(ticket: Ticket, page: &Inventory) -> Result<Self, Error> {
        if page.ids.len() > MAX_REPLICA_INVENTORY_PAGE
            || !same_scope(page.base, page.observed)
            || page.base.sequence > page.observed.sequence
        {
            return Err(Error::Invalid);
        }
        if let Some(next) = page.next {
            if next.base != page.base
                || page.ids.last() != Some(&next.after)
                || next.after >= next.upper
                || page.ids.iter().any(|id| *id > next.upper)
            {
                return Err(Error::Invalid);
            }
        }
        Self::new(
            ticket,
            page.observed,
            &page.ids,
            Advance::Inventory(page.next),
        )
    }
    pub fn changes(ticket: Ticket, page: &Changes<EmailId>) -> Result<Self, Error> {
        if !same_scope(page.old, page.new) || page.new.sequence < page.old.sequence {
            return Err(Error::Invalid);
        }
        let count = page
            .created
            .len()
            .checked_add(page.updated.len())
            .and_then(|n| n.checked_add(page.destroyed.len()))
            .ok_or(Error::Capacity)?;
        if count > MAX_IDS {
            return Err(Error::Capacity);
        }
        if (count != 0 || page.more) && page.new.sequence == page.old.sequence {
            return Err(Error::Invalid);
        }
        let mut ids = Vec::new();
        ids.try_reserve_exact(count).map_err(|_| Error::Capacity)?;
        ids.extend(
            page.created
                .iter()
                .chain(&page.updated)
                .chain(&page.destroyed)
                .copied(),
        );
        ids.sort_unstable();
        Self::new(
            ticket,
            page.new,
            &ids,
            Advance::Changes {
                state: page.new,
                more: page.more,
            },
        )
    }
    fn new(
        ticket: Ticket,
        observed: DataState,
        ids: &[EmailId],
        advance: Advance,
    ) -> Result<Self, Error> {
        ticket.validate()?;
        if observed.kind != DataType::Email
            || ids.len() > MAX_IDS
            || ids.windows(2).any(|pair| pair.first() >= pair.get(1))
        {
            return Err(Error::Invalid);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(ids.len())
            .map_err(|_| Error::Capacity)?;
        owned.extend_from_slice(ids);
        Ok(Self {
            ticket,
            observed,
            ids: owned,
            done: [0; DONE_BYTES],
            advance,
        })
    }
    pub fn ticket(&self) -> Ticket {
        self.ticket
    }
    pub fn observed(&self) -> DataState {
        self.observed
    }
    pub fn ids(&self) -> &[EmailId] {
        &self.ids
    }
    pub fn done(&self) -> &[u8; DONE_BYTES] {
        &self.done
    }
    pub fn advance(&self) -> Advance {
        self.advance
    }
    pub fn complete(&self) -> bool {
        self.ids
            .iter()
            .enumerate()
            .all(|(index, _)| self.finished(index))
    }
    pub fn unfinished(&self) -> impl Iterator<Item = EmailId> + '_ {
        self.ids
            .iter()
            .enumerate()
            .filter_map(|(index, id)| (!self.finished(index)).then_some(*id))
    }
    fn finished(&self, index: usize) -> bool {
        self.done
            .get(index / 8)
            .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
    }
    /// Call only inside the same local transaction that applied the observed
    /// metadata/body or authoritative absence. Persist this bitmap atomically.
    pub fn applied(
        &mut self,
        ticket: Ticket,
        id: EmailId,
        observed: DataState,
    ) -> Result<(), Error> {
        if ticket != self.ticket {
            return Err(Error::Stale);
        }
        if !same_scope(observed, self.observed) {
            return Err(Error::Scope);
        }
        if observed.sequence < self.observed.sequence {
            return Err(Error::Stale);
        }
        let index = self.ids.binary_search(&id).map_err(|_| Error::Missing)?;
        let byte = self.done.get_mut(index / 8).ok_or(Error::Invalid)?;
        *byte |= 1 << (index % 8);
        Ok(())
    }
    /// Exact bounded storage intake; unused bits must remain zero.
    pub fn restore_done(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let done: [u8; DONE_BYTES] = bytes.try_into().map_err(|_| Error::Invalid)?;
        for index in self.ids.len()..DONE_BYTES * 8 {
            if done
                .get(index / 8)
                .is_none_or(|byte| byte & (1 << (index % 8)) != 0)
            {
                return Err(Error::Invalid);
            }
        }
        self.done = done;
        Ok(())
    }
}
fn same_scope(a: DataState, b: DataState) -> bool {
    a.kind == DataType::Email
        && b.kind == DataType::Email
        && a.account == b.account
        && a.epoch == b.epoch
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        format::Sequence,
        ids::{AccountId, StoreEpoch},
    };
    fn state(n: u64) -> DataState {
        DataState {
            epoch: StoreEpoch::from_bytes([1; 16]),
            account: AccountId::from_bytes([2; 16]),
            kind: DataType::Email,
            sequence: Sequence::from_u64(n),
        }
    }
    fn ticket() -> Ticket {
        Ticket {
            generation: 3,
            page: 1,
        }
    }
    fn inventory() -> Inventory {
        Inventory {
            previous: None,
            base: state(5),
            observed: state(5),
            ids: vec![EmailId::from_bytes([0; 16]), EmailId::from_bytes([1; 16])],
            next: Some(ReplicaInventoryCursor {
                base: state(5),
                after: EmailId::from_bytes([1; 16]),
                upper: EmailId::from_bytes([9; 16]),
            }),
        }
    }
    #[test]
    fn interrupted_first_page_preserves_zero_id_and_only_durable_effects() {
        let page = inventory();
        let mut pending = Pending::inventory(ticket(), &page).unwrap();
        assert!(!pending.complete());
        pending.applied(ticket(), page.ids[0], state(7)).unwrap();
        let durable = *pending.done();
        // A second effect whose transaction did not commit must not survive.
        pending.applied(ticket(), page.ids[1], state(8)).unwrap();
        assert!(pending.complete());
        let mut resumed = Pending::inventory(ticket(), &page).unwrap();
        resumed.restore_done(&durable).unwrap();
        assert_eq!(resumed.unfinished().collect::<Vec<_>>(), vec![page.ids[1]]);
        assert!(!resumed.complete());
        resumed.applied(ticket(), page.ids[0], state(9)).unwrap();
        assert!(!resumed.complete());
        resumed.applied(ticket(), page.ids[1], state(9)).unwrap();
        assert!(resumed.complete());
        assert_eq!(resumed.advance(), Advance::Inventory(page.next));
    }
    #[test]
    fn abandoned_generation_old_observation_and_wrong_scope_cannot_complete_work() {
        let page = inventory();
        let mut pending = Pending::inventory(ticket(), &page).unwrap();
        for old in [
            Ticket {
                generation: 2,
                page: 1,
            },
            Ticket {
                generation: 3,
                page: 2,
            },
        ] {
            assert_eq!(
                pending.applied(old, page.ids[0], state(6)),
                Err(Error::Stale)
            );
        }
        assert_eq!(
            pending.applied(ticket(), page.ids[0], state(4)),
            Err(Error::Stale)
        );
        let mut wrong = state(6);
        wrong.account = AccountId::from_bytes([7; 16]);
        assert_eq!(
            pending.applied(ticket(), page.ids[0], wrong),
            Err(Error::Scope)
        );
        wrong = state(6);
        wrong.epoch = StoreEpoch::from_bytes([7; 16]);
        assert_eq!(
            pending.applied(ticket(), page.ids[0], wrong),
            Err(Error::Scope)
        );
        wrong = state(6);
        wrong.kind = DataType::Mailbox;
        assert_eq!(
            pending.applied(ticket(), page.ids[0], wrong),
            Err(Error::Scope)
        );
        assert_eq!(
            pending.applied(ticket(), EmailId::from_bytes([8; 16]), state(6)),
            Err(Error::Missing)
        );
        assert_eq!(pending.unfinished().count(), 2);
    }
    #[test]
    fn change_checkpoint_waits_for_all_created_updated_and_destroyed_ids() {
        let page = Changes {
            old: state(5),
            new: state(9),
            more: true,
            created: vec![EmailId::from_bytes([3; 16])],
            updated: vec![EmailId::from_bytes([1; 16])],
            destroyed: vec![EmailId::from_bytes([2; 16])],
        };
        let mut pending = Pending::changes(ticket(), &page).unwrap();
        assert_eq!(
            pending.ids(),
            &[
                EmailId::from_bytes([1; 16]),
                EmailId::from_bytes([2; 16]),
                EmailId::from_bytes([3; 16])
            ]
        );
        pending
            .applied(ticket(), page.created[0], state(10))
            .unwrap();
        pending
            .applied(ticket(), page.updated[0], state(10))
            .unwrap();
        assert!(!pending.complete());
        pending
            .applied(ticket(), page.destroyed[0], state(11))
            .unwrap();
        assert!(pending.complete());
        assert_eq!(
            pending.advance(),
            Advance::Changes {
                state: state(9),
                more: true
            }
        );
        // A newer per-object observation is never the global checkpoint.
        assert_eq!(pending.observed(), state(9));
    }
    #[test]
    fn malformed_pages_and_completion_padding_refuse() {
        let mut page = inventory();
        page.ids.reverse();
        assert!(matches!(
            Pending::inventory(ticket(), &page),
            Err(Error::Invalid)
        ));
        let mut pending = Pending::inventory(ticket(), &inventory()).unwrap();
        let mut done = [0; DONE_BYTES];
        done[0] = 4;
        assert_eq!(pending.restore_done(&done), Err(Error::Invalid));
        done[0] = 0;
        done[DONE_BYTES - 1] = 128;
        assert_eq!(pending.restore_done(&done), Err(Error::Invalid));
        assert_eq!(pending.restore_done(&[]), Err(Error::Invalid));
        let id = EmailId::from_bytes([1; 16]);
        let page = Changes {
            old: state(5),
            new: state(6),
            more: false,
            created: vec![id],
            updated: vec![],
            destroyed: vec![id],
        };
        assert!(matches!(
            Pending::changes(ticket(), &page),
            Err(Error::Invalid)
        ));
    }
}
