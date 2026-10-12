#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::{
        key::Key,
        row::{MailboxRow, Row},
        ObjectType, Sequence, Table,
    },
    ids::{AccountId, StoreEpoch},
    ports::{self, ChangeCursor, ChangeStep, Record},
};
fn id(n: u8) -> MailboxId {
    MailboxId::from_bytes([n; 16])
}
fn identity() -> ViewIdentity {
    ViewIdentity {
        account: AccountId::from_bytes([1; 16]),
        epoch: StoreEpoch::from_bytes([2; 16]),
        committed_sequence: Sequence::from_u64(3),
        history_floor: Sequence::from_u64(0),
    }
}
struct View {
    parents: Vec<Option<u8>>,
    identity: ViewIdentity,
    reads: usize,
    fail: Option<ports::Error>,
    change: bool,
}
impl View {
    fn new(parents: Vec<Option<u8>>) -> Self {
        Self {
            parents,
            identity: identity(),
            reads: 0,
            fail: None,
            change: false,
        }
    }
}
impl ReadView for View {
    fn identity(&self) -> ViewIdentity {
        self.identity
    }
    fn next_change(&mut self, _: ChangeCursor, _: ObjectType) -> Result<ChangeStep, ports::Error> {
        Err(ports::Error::Invalid)
    }
    fn next<'a>(
        &mut self,
        _: Table,
        _: Option<&[u8]>,
        _: &'a mut [u8],
        _: &'a mut [u8],
    ) -> Result<Option<Record<'a>>, ports::Error> {
        Err(ports::Error::Invalid)
    }
    fn get<'a>(
        &mut self,
        key: Key<'_>,
        value: &'a mut [u8],
    ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
        self.reads += 1;
        if self.change {
            self.identity.epoch = StoreEpoch::from_bytes([99; 16]);
        }
        if let Some(error) = self.fail {
            return Err(error);
        }
        let Key::Mailbox(mailbox) = key else {
            return Err(ports::Error::Invalid);
        };
        let Some(parent) = self.parents.get(mailbox.as_bytes()[0] as usize) else {
            return Ok(None);
        };
        let row = Row::Mailbox(MailboxRow {
            name: "Folder",
            parent: parent.map(id),
            role: None,
            sort_order: 0,
            subscribed: true,
        });
        let len = row.encode(value).map_err(|_| ports::Error::Capacity)?;
        Ok(Some((
            Row::decode(Table::Mailboxes, &value[..len]).unwrap(),
            self.identity.committed_sequence,
        )))
    }
}
fn path(view: &mut View, start: u8) -> Result<Path, Error> {
    let mut walk = Walk::new(view.identity(), id(start))?;
    let mut value = [0; 128];
    while !walk.is_complete() {
        let before = view.reads;
        walk.advance(view, &mut value)?;
        assert_eq!(view.reads, before + 1);
    }
    let before = view.reads;
    walk.advance(view, &mut value)?;
    assert_eq!(view.reads, before);
    walk.finish()
}
#[test]
fn exact_depth_root_paths_and_ancestor_relations() {
    let mut view = View::new((0..32u8).map(|n| n.checked_sub(1)).collect());
    for n in 0..32u8 {
        let result = path(&mut view, n).unwrap();
        assert_eq!(
            result.from_root().collect::<Vec<_>>(),
            (0..=n).map(id).collect::<Vec<_>>()
        );
        assert_eq!(result.identity(), view.identity());
        for other in 0..32u8 {
            let second = path(&mut view, other).unwrap();
            assert_eq!(
                result.relation(&second).unwrap(),
                Relation::Ordered(n.cmp(&other))
            );
        }
    }
    view.parents.push(Some(31));
    let before = view.reads;
    assert!(matches!(path(&mut view, 32), Err(Error::ReadLimit)));
    assert_eq!(view.reads - before, 32);
    assert!(std::mem::size_of::<Walk>() < 1024);
}
#[test]
fn branch_relations_select_nearest_siblings_not_leaf_ids() {
    // 0 -> 1 -> 3; 0 -> 2 -> 4 -> 5; separate root 6 -> 7.
    let mut view = View::new(vec![
        None,
        Some(0),
        Some(0),
        Some(1),
        Some(2),
        Some(4),
        None,
        Some(6),
    ]);
    for (a, b, left, right) in [
        (3, 5, 1, 2),
        (5, 3, 2, 1),
        (3, 7, 0, 6),
        (7, 5, 6, 0),
        (1, 2, 1, 2),
    ] {
        let a = path(&mut view, a).unwrap();
        let b = path(&mut view, b).unwrap();
        assert_eq!(
            a.relation(&b).unwrap(),
            Relation::Siblings(id(left), id(right))
        );
    }
}
#[test]
fn all_small_acyclic_forests_match_independent_root_vectors() {
    for n in 1..=5u8 {
        for mut code in 0..(u32::from(n) + 1).pow(u32::from(n)) {
            let mut parents = Vec::new();
            for node in 0..n {
                let digit = (code % (u32::from(n) + 1)) as u8;
                code /= u32::from(n) + 1;
                parents.push(if digit < node { Some(digit) } else { None });
            }
            let expected: Vec<Vec<u8>> = (0..n)
                .map(|mut at| {
                    let mut chain = vec![at];
                    while let Some(parent) = parents[at as usize] {
                        chain.push(parent);
                        at = parent;
                    }
                    chain.reverse();
                    chain
                })
                .collect();
            let mut view = View::new(parents);
            let paths: Vec<_> = (0..n).map(|at| path(&mut view, at).unwrap()).collect();
            for a in 0..n as usize {
                for b in 0..n as usize {
                    let common = expected[a]
                        .iter()
                        .zip(&expected[b])
                        .take_while(|(a, b)| a == b)
                        .count();
                    let relation = if common == expected[a].len() || common == expected[b].len() {
                        Relation::Ordered(expected[a].len().cmp(&expected[b].len()))
                    } else {
                        Relation::Siblings(id(expected[a][common]), id(expected[b][common]))
                    };
                    assert_eq!(paths[a].relation(&paths[b]).unwrap(), relation);
                }
            }
        }
    }
}
#[test]
fn incomplete_faulted_and_changed_views_never_expose_partial_paths() {
    assert!(matches!(
        Walk::new(identity(), id(0)).unwrap().finish(),
        Err(Error::Incomplete)
    ));
    for (parents, error, reads) in [
        (vec![Some(1), Some(0)], Error::Cycle, 3),
        (vec![Some(2)], Error::Missing(id(2)), 2),
        (
            vec![Some(0)],
            Error::Format(crate::format::Error::InvalidValue),
            1,
        ),
    ] {
        let mut view = View::new(parents);
        assert_eq!(path(&mut view, 0).unwrap_err(), error);
        assert_eq!(view.reads, reads);
    }
    for failure in [
        ports::Error::Deadline,
        ports::Error::Capacity,
        ports::Error::Busy,
    ] {
        let mut view = View::new(vec![None]);
        view.fail = Some(failure);
        let mut walk = Walk::new(identity(), id(0)).unwrap();
        assert_eq!(
            walk.advance(&mut view, &mut [0; 128]),
            Err(Error::View(failure))
        );
        view.fail = None;
        assert!(walk.is_failed());
        assert_eq!(walk.advance(&mut view, &mut [0; 128]), Err(Error::Failed));
        assert!(matches!(walk.finish(), Err(Error::Failed)));
        assert_eq!(view.reads, 1);
    }
    let mut view = View::new(vec![None]);
    let original = path(&mut view, 0).unwrap();
    view.identity.account = AccountId::from_bytes([9; 16]);
    let different = path(&mut view, 0).unwrap();
    assert_eq!(original.relation(&different), Err(Error::ChangedView));
    let mut view = View::new(vec![None]);
    view.change = true;
    assert!(matches!(path(&mut view, 0), Err(Error::ChangedView)));
}
