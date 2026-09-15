//! Visible, sibling-sorted projection of one immutable process snapshot.
use crate::budget::{Budget, Error as BudgetError, MemoryVec};
use crate::hierarchy::{ParentIssue, ProcessKey, DEPTH, ROWS};
use crate::snapshot::{Names, Snapshot};
use std::cmp::Ordering;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Key {
    Unavailable,
    Process(ProcessKey),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Column {
    Name,
    Pid,
    Uid,
    State,
    Cpu,
    Rss,
    TreeCpu,
    TreeRss,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sort {
    pub column: Column,
    pub descending: bool,
}
impl Default for Sort {
    fn default() -> Self {
        Self {
            column: Column::Pid,
            descending: false,
        }
    }
}
impl Sort {
    pub fn flat(self) -> bool {
        matches!(
            self.column,
            Column::Cpu | Column::Rss | Column::TreeCpu | Column::TreeRss
        )
    }
    pub fn click(&mut self, column: Column) {
        *self = Self {
            column,
            descending: if self.column == column {
                !self.descending
            } else {
                !matches!(column, Column::Name | Column::State)
            },
        };
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row {
    pub key: Key,
    pub parent: Option<Key>,
    pub depth: u16,
    pub children: bool,
    pub expanded: bool,
    pub process: Option<usize>,
    pub context: bool,
    pub exception: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Budget(BudgetError),
    Invalid,
    Names,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => e.fmt(f),
            Self::Invalid => f.write_str("invalid process projection"),
            Self::Names => f.write_str("process names unavailable"),
        }
    }
}
impl std::error::Error for Error {}
impl From<BudgetError> for Error {
    fn from(e: BudgetError) -> Self {
        Self::Budget(e)
    }
}
fn push<T>(v: &mut MemoryVec<T>, value: T) -> Result<(), Error> {
    v.push(value).map_err(|_| Error::Invalid)
}
fn filled<T: Copy>(budget: &Arc<Budget>, count: usize, value: T) -> Result<MemoryVec<T>, Error> {
    let mut result = MemoryVec::new(budget, count)?;
    for _ in 0..count {
        push(&mut result, value)?;
    }
    Ok(result)
}
/// The expanded default stores only explicit collapses, bounded by the roster.
#[derive(Debug)]
pub struct Expansion {
    collapsed: MemoryVec<Key>,
}
impl Expansion {
    pub fn new(budget: &Arc<Budget>) -> Result<Self, Error> {
        Ok(Self {
            collapsed: MemoryVec::new(budget, 64)?,
        })
    }
    pub fn expanded(&self, key: Key) -> bool {
        self.collapsed.binary_search(&key).is_err()
    }
    pub fn set(&mut self, key: Key, expanded: bool) -> Result<(), Error> {
        match (self.collapsed.binary_search(&key), expanded) {
            (Ok(index), true) => {
                self.collapsed.remove(index);
            }
            (Err(_), false) => {
                if self.collapsed.len() > ROWS {
                    return Err(Error::Invalid);
                }
                if self.collapsed.len() == self.collapsed.capacity() {
                    self.collapsed
                        .reserve((self.collapsed.len() * 2).min(ROWS + 1))?;
                }
                push(&mut self.collapsed, key)?;
                self.collapsed.sort_unstable();
            }
            _ => {}
        }
        Ok(())
    }
    pub fn retain_snapshot(&mut self, snapshot: &Snapshot) {
        self.collapsed.retain(|key| match key {
            Key::Unavailable => true,
            Key::Process(key) => snapshot
                .processes()
                .binary_search_by_key(key, |p| p.key)
                .is_ok(),
        });
    }
    pub fn reveal(&mut self, snapshot: &Snapshot, key: ProcessKey) {
        let Ok(mut index) = snapshot.processes().binary_search_by_key(&key, |p| p.key) else {
            return;
        };
        for _ in 0..=DEPTH {
            let Some(node) = snapshot.ancestry().get(index) else {
                break;
            };
            if let Some(parent) = node.parent {
                if let Some(process) = snapshot.processes().get(parent) {
                    let _ = self.set(Key::Process(process.key), true);
                }
                index = parent;
            } else {
                if node.issue != ParentIssue::None {
                    let _ = self.set(Key::Unavailable, true);
                }
                break;
            }
        }
    }
}
#[derive(Debug)]
pub struct Projection {
    rows: MemoryVec<Row>,
}
fn compare(snapshot: &Snapshot, names: &Names<'_>, a: usize, b: usize, sort: Sort) -> Ordering {
    let Some((a_row, b_row)) = snapshot.processes().get(a).zip(snapshot.processes().get(b)) else {
        return a.cmp(&b);
    };
    let order = match sort.column {
        Column::Name => names
            .get(a)
            .map(|n| n.name())
            .cmp(&names.get(b).map(|n| n.name())),
        Column::Pid => a_row.key.pid.cmp(&b_row.key.pid),
        Column::Uid => a_row.uid.cmp(&b_row.uid),
        Column::State => a_row.state.cmp(&b_row.state),
        Column::Cpu => a_row.cpu.cmp(&b_row.cpu),
        Column::Rss => a_row.rss.cmp(&b_row.rss),
        Column::TreeCpu => snapshot
            .ancestry()
            .get(a)
            .and_then(|n| n.cpu.value())
            .cmp(&snapshot.ancestry().get(b).and_then(|n| n.cpu.value())),
        Column::TreeRss => snapshot
            .ancestry()
            .get(a)
            .and_then(|n| n.rss.value())
            .cmp(&snapshot.ancestry().get(b).and_then(|n| n.rss.value())),
    };
    (if sort.descending {
        order.reverse()
    } else {
        order
    })
    .then_with(|| a_row.key.cmp(&b_row.key))
}
fn decimal_contains(mut number: u32, query: &str) -> bool {
    let mut bytes = [0; 10];
    let mut start = bytes.len();
    loop {
        start = start.saturating_sub(1);
        if let Some(slot) = bytes.get_mut(start) {
            *slot = b'0' + (number % 10) as u8;
        }
        number /= 10;
        if number == 0 {
            break;
        }
    }
    bytes
        .get(start..)
        .and_then(|b| std::str::from_utf8(b).ok())
        .is_some_and(|text| text.contains(query))
}
fn matches_query(snapshot: &Snapshot, names: &Names<'_>, index: usize, query: &str) -> bool {
    query.is_empty()
        || names
            .get(index)
            .is_some_and(|name| name.name().contains(query))
        || snapshot.processes().get(index).is_some_and(|p| {
            decimal_contains(p.key.pid, query)
                || p.uid.is_some_and(|uid| decimal_contains(uid, query))
        })
}
fn parent(snapshot: &Snapshot, index: usize) -> Option<usize> {
    let node = snapshot.ancestry().get(index)?;
    node.parent
        .or_else(|| (node.issue != ParentIssue::None).then_some(snapshot.processes().len()))
}
fn key(snapshot: &Snapshot, index: usize) -> Option<Key> {
    if index == snapshot.processes().len() {
        Some(Key::Unavailable)
    } else {
        snapshot.processes().get(index).map(|p| Key::Process(p.key))
    }
}
impl Projection {
    pub fn new(
        budget: &Arc<Budget>,
        snapshot: &Snapshot,
        sort: Sort,
        query: &str,
        selected: Option<ProcessKey>,
        expansion: &Expansion,
    ) -> Result<Self, Error> {
        if query.len() > 256 {
            return Err(Error::Invalid);
        }
        snapshot
            .with_identities(|names| {
                Self::build(budget, snapshot, sort, query, selected, expansion, &names)
            })
            .map_err(|_| Error::Names)?
    }
    #[allow(clippy::too_many_arguments)]
    fn build(
        budget: &Arc<Budget>,
        snapshot: &Snapshot,
        sort: Sort,
        query: &str,
        selected: Option<ProcessKey>,
        expansion: &Expansion,
        names: &Names<'_>,
    ) -> Result<Self, Error> {
        let count = snapshot.processes().len();
        let mut order = MemoryVec::new(budget, count + 1)?;
        if sort.flat() {
            let mut rows = MemoryVec::new(budget, count)?;
            for (index, process) in snapshot.processes().iter().enumerate() {
                let matching = matches_query(snapshot, names, index, query);
                if matching || selected == Some(process.key) {
                    push(&mut order, index)?;
                }
            }
            order.sort_unstable_by(|a, b| compare(snapshot, names, *a, *b, sort));
            for index in order.iter().copied() {
                push(
                    &mut rows,
                    Row {
                        key: key(snapshot, index).ok_or(Error::Invalid)?,
                        parent: None,
                        depth: 0,
                        children: false,
                        expanded: false,
                        process: Some(index),
                        context: false,
                        exception: snapshot
                            .processes()
                            .get(index)
                            .is_some_and(|p| selected == Some(p.key))
                            && !matches_query(snapshot, names, index, query),
                    },
                )?;
            }
            return Ok(Self { rows });
        }
        let mut synthetic = false;
        let mut matches = filled(budget, count + 1, false)?;
        let mut kept = filled(budget, count + 1, false)?;
        let mut exceptions = filled(budget, count + 1, false)?;
        for (index, process) in snapshot.processes().iter().enumerate() {
            push(&mut order, index)?;
            synthetic |= parent(snapshot, index) == Some(count);
            let matching = matches_query(snapshot, names, index, query);
            *matches.get_mut(index).ok_or(Error::Invalid)? = matching;
            let exception = selected == Some(process.key) && !matching;
            if matching || exception {
                let mut ancestor = Some(index);
                for _ in 0..=DEPTH {
                    let Some(at) = ancestor else { break };
                    *kept.get_mut(at).ok_or(Error::Invalid)? = true;
                    if exception && at == index {
                        *exceptions.get_mut(at).ok_or(Error::Invalid)? = true;
                    }
                    ancestor = parent(snapshot, at);
                }
            }
        }
        if synthetic {
            push(&mut order, count)?;
        }
        order.sort_unstable_by(|a, b| {
            parent(snapshot, *a)
                .cmp(&parent(snapshot, *b))
                .then_with(|| {
                    if *a == count || *b == count {
                        a.cmp(b)
                    } else {
                        compare(snapshot, names, *a, *b, sort)
                    }
                })
        });
        let mut first = filled(budget, count + 2, None)?;
        let mut next = filled(budget, count + 1, None)?;
        let root = count + 1;
        for index in order.iter().rev().copied() {
            if !kept.get(index).copied().unwrap_or(false) {
                continue;
            }
            let group = parent(snapshot, index).unwrap_or(root);
            let head = first.get_mut(group).ok_or(Error::Invalid)?;
            *next.get_mut(index).ok_or(Error::Invalid)? = *head;
            *head = Some(index);
        }
        let mut rows = MemoryVec::new(budget, count + usize::from(synthetic))?;
        let mut stack = MemoryVec::new(budget, DEPTH + 2)?;
        let mut current = first.get(root).copied().flatten();
        let mut depth = 0u16;
        while let Some(index) = current {
            let id = key(snapshot, index).ok_or(Error::Invalid)?;
            let child = first.get(index).copied().flatten();
            let expanded = expansion.expanded(id) || (!query.is_empty() && child.is_some());
            push(
                &mut rows,
                Row {
                    key: id,
                    parent: parent(snapshot, index).and_then(|p| key(snapshot, p)),
                    depth,
                    children: child.is_some(),
                    expanded,
                    process: (index < count).then_some(index),
                    context: !query.is_empty() && !matches.get(index).copied().unwrap_or(false),
                    exception: exceptions.get(index).copied().unwrap_or(false),
                },
            )?;
            let sibling = next.get(index).copied().flatten();
            if expanded && child.is_some() {
                push(&mut stack, sibling)?;
                depth = depth
                    .checked_add(1)
                    .filter(|d| usize::from(*d) <= DEPTH)
                    .ok_or(Error::Invalid)?;
                current = child;
            } else {
                current = sibling;
                while current.is_none() {
                    let Some(sibling) = stack.pop() else { break };
                    depth = depth.saturating_sub(1);
                    current = sibling;
                }
            }
        }
        Ok(Self { rows })
    }
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }
}
