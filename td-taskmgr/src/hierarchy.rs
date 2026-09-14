//! Snapshot-local ancestry and totals; display keys carry no authority.
use crate::budget::{Budget, Error as BudgetError, MemoryVec};
use std::sync::Arc;
pub const ROWS: usize = 32768;
pub const DEPTH: usize = 256;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ProcessKey {
    pub generation: u64,
    pub pid: u32,
    pub start_ticks: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Input {
    pub key: ProcessKey,
    pub parent_pid: Option<u32>,
    pub cpu: Option<u64>,
    pub rss: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParentIssue {
    None,
    Unavailable,
    Cycle,
    Depth,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Total {
    sum: u64,
    known: bool,
    overflow: bool,
    pub partial: bool,
}
impl Total {
    pub fn new(value: Option<u64>, partial: bool) -> Self {
        Self {
            sum: value.unwrap_or(0),
            known: value.is_some(),
            overflow: false,
            partial: partial || value.is_none(),
        }
    }
    pub fn value(self) -> Option<u64> {
        (self.known && !self.overflow).then_some(self.sum)
    }
    fn add(&mut self, other: Self) {
        self.known |= other.known;
        self.partial |= other.partial;
        self.overflow |= other.overflow;
        if let Some(sum) = self.sum.checked_add(other.sum) {
            self.sum = sum;
        } else {
            self.overflow = true;
            self.partial = true;
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Node {
    pub parent: Option<usize>,
    pub depth: u16,
    pub issue: ParentIssue,
    pub cpu: Total,
    pub rss: Total,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRows,
    Budget(BudgetError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRows => f.write_str("invalid snapshot process roster"),
            Self::Budget(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for Error {}
fn push<T>(rows: &mut MemoryVec<T>, value: T) -> Result<(), Error> {
    rows.push(value)
        .map_err(|_| Error::Budget(BudgetError::Limit))
}
#[derive(Debug)]
pub struct Forest {
    nodes: MemoryVec<Node>,
}
impl Forest {
    /// Inputs have unique increasing PIDs and one collection generation.
    pub fn new(budget: &Arc<Budget>, rows: &[Input], partial: bool) -> Result<Self, Error> {
        if rows.len() > ROWS
            || rows.iter().any(|row| row.key.pid == 0)
            || rows.windows(2).any(|pair| {
                pair.first().zip(pair.get(1)).is_some_and(|(a, b)| {
                    a.key.pid >= b.key.pid || a.key.generation != b.key.generation
                })
            })
        {
            return Err(Error::InvalidRows);
        }
        let mut nodes = MemoryVec::new(budget, rows.len()).map_err(Error::Budget)?;
        for row in rows {
            let (parent, issue) = match row.parent_pid {
                Some(0) => (None, ParentIssue::None),
                Some(pid) if pid == row.key.pid => (None, ParentIssue::Cycle),
                Some(pid) => match rows.binary_search_by_key(&pid, |row| row.key.pid) {
                    Ok(parent) => (Some(parent), ParentIssue::None),
                    Err(_) => (None, ParentIssue::Unavailable),
                },
                None => (None, ParentIssue::Unavailable),
            };
            push(
                &mut nodes,
                Node {
                    parent,
                    depth: 0,
                    issue,
                    cpu: Total::new(row.cpu, partial),
                    rss: Total::new(row.rss, partial),
                },
            )?;
        }
        let mut done = MemoryVec::new(budget, rows.len()).map_err(Error::Budget)?;
        for _ in rows {
            push(&mut done, false)?;
        }
        let mut path: MemoryVec<usize> = MemoryVec::new(budget, DEPTH).map_err(Error::Budget)?;
        for start in 0..rows.len() {
            if done.get(start).copied().unwrap_or(false) {
                continue;
            }
            path.clear();
            let mut current = start;
            loop {
                if done.get(current).copied().unwrap_or(false) {
                    break;
                }
                if let Some(cycle) = path.iter().position(|index| *index == current) {
                    let cut = path
                        .get(cycle..)
                        .and_then(|cycle| cycle.iter().copied().min())
                        .ok_or(Error::InvalidRows)?;
                    let node = nodes.get_mut(cut).ok_or(Error::InvalidRows)?;
                    node.parent = None;
                    node.issue = ParentIssue::Cycle;
                    // Rewalk the original chain after removing the cycle edge.
                    path.clear();
                    current = start;
                    continue;
                }
                if path.len() == DEPTH {
                    let node = nodes.get_mut(current).ok_or(Error::InvalidRows)?;
                    node.parent = None;
                    node.issue = ParentIssue::Depth;
                    *done.get_mut(current).ok_or(Error::InvalidRows)? = true;
                    break;
                }
                push(&mut path, current)?;
                match nodes.get(current).and_then(|node| node.parent) {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
            while let Some(index) = path.pop() {
                let parent = nodes.get(index).and_then(|node| node.parent);
                let depth = parent
                    .and_then(|parent| nodes.get(parent))
                    .map(|parent| usize::from(parent.depth) + 1)
                    .unwrap_or(0);
                let node = nodes.get_mut(index).ok_or(Error::InvalidRows)?;
                if depth >= DEPTH {
                    node.parent = None;
                    node.issue = ParentIssue::Depth;
                    node.depth = 0;
                } else {
                    node.depth = depth as u16;
                }
                *done.get_mut(index).ok_or(Error::InvalidRows)? = true;
            }
        }
        let mut order = MemoryVec::new(budget, nodes.len()).map_err(Error::Budget)?;
        for index in 0..nodes.len() {
            push(&mut order, index)?;
        }
        order.sort_unstable_by_key(|index| {
            std::cmp::Reverse(nodes.get(*index).map(|node| node.depth).unwrap_or(0))
        });
        for index in order.iter().copied() {
            let node = nodes.get(index).copied().ok_or(Error::InvalidRows)?;
            if let Some(parent) = node.parent {
                let ancestor = nodes.get_mut(parent).ok_or(Error::InvalidRows)?;
                ancestor.cpu.add(node.cpu);
                ancestor.rss.add(node.rss);
            }
        }
        Ok(Self { nodes })
    }
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
}
