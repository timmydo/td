//! Pure validation of proc status and deterministic observed subtree membership.
use crate::actions::{Scope, TARGETS};
use crate::budget::{Budget, MemoryVec};
use crate::hierarchy::{Input, ProcessKey};
use crate::parsers;
use std::sync::Arc;

type Result<T> = std::result::Result<T, &'static str>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status {
    pub pid: u32,
    pub group: u32,
    pub uid: u32,
    pub namespaces: usize,
    pub namespace_init: bool,
}
fn number(bytes: &[u8]) -> Option<u32> {
    parsers::unsigned(bytes).and_then(|n| u32::try_from(n).ok())
}
fn words(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(u8::is_ascii_whitespace)
        .filter(|s| !s.is_empty())
}
pub fn status(bytes: &[u8]) -> Result<Status> {
    if bytes.len() > parsers::PROCESS_BYTES {
        return Err("status exceeds byte limit");
    }
    let mut pid = None;
    let mut group = None;
    let mut uid = None;
    let mut namespace = None;
    for line in bytes.split(|b| *b == b'\n') {
        for (label, slot) in [
            (b"Pid:".as_slice(), &mut pid),
            (b"Tgid:".as_slice(), &mut group),
        ] {
            if let Some(tail) = line.strip_prefix(label) {
                let mut fields = words(tail);
                let value = fields
                    .next()
                    .and_then(number)
                    .filter(|n| *n > 0)
                    .ok_or("invalid status identity")?;
                if fields.next().is_some() || slot.replace(value).is_some() {
                    return Err("ambiguous status identity");
                }
            }
        }
        if let Some(tail) = line.strip_prefix(b"Uid:") {
            let mut fields = words(tail);
            let real = fields
                .next()
                .and_then(number)
                .ok_or("invalid process UID")?;
            for _ in 0..3 {
                fields
                    .next()
                    .and_then(number)
                    .ok_or("invalid process UID")?;
            }
            if fields.next().is_some() || uid.replace(real).is_some() {
                return Err("ambiguous process UID");
            }
        }
        if let Some(tail) = line.strip_prefix(b"NStgid:") {
            let mut first = None;
            let mut count = 0;
            let mut init = false;
            for field in words(tail) {
                let value = number(field)
                    .filter(|n| *n > 0)
                    .ok_or("invalid PID namespace view")?;
                first.get_or_insert(value);
                init |= value == 1;
                count += 1;
                if count > 32 {
                    return Err("PID namespace depth exceeds limit");
                }
            }
            let first = first.ok_or("missing PID namespace view")?;
            if namespace.replace((first, count, init)).is_some() {
                return Err("ambiguous PID namespace view");
            }
        }
    }
    let pid = pid.ok_or("missing process identity")?;
    let group = group
        .filter(|group| *group == pid)
        .ok_or("target is not a process leader")?;
    let uid = uid.ok_or("missing process UID")?;
    let (first, namespaces, namespace_init) = namespace.ok_or("missing PID namespace view")?;
    if first != pid {
        return Err("inconsistent PID namespace view");
    }
    Ok(Status {
        pid,
        group,
        uid,
        namespaces,
        namespace_init,
    })
}
pub fn caller(status: Status, own_pid: u32) -> Result<()> {
    if status.pid != own_pid || status.namespaces != 1 {
        return Err("procfs PID view is not the caller's own namespace");
    }
    Ok(())
}
pub fn protected(status: Status, own_pid: u32) -> Result<()> {
    if status.pid == own_pid {
        return Err("the task manager is protected");
    }
    if status.namespace_init {
        return Err("namespace PID 1 is protected");
    }
    Ok(())
}
/// Inputs have unique increasing PIDs; the fresh complete scan is the caller's.
pub fn members(
    budget: &Arc<Budget>,
    rows: &[Input],
    root: ProcessKey,
    scope: Scope,
) -> Result<MemoryVec<usize>> {
    if rows.windows(2).any(|pair| {
        pair.first()
            .zip(pair.get(1))
            .is_some_and(|(a, b)| a.key.pid >= b.key.pid)
    }) {
        return Err("process roster is not unique and ordered");
    }
    let at = rows
        .binary_search_by_key(&root.pid, |row| row.key.pid)
        .ok()
        .filter(|at| rows.get(*at).is_some_and(|row| row.key == root))
        .ok_or("selected process is no longer the observed identity")?;
    let mut selected =
        MemoryVec::new(budget, TARGETS).map_err(|_| "action memory budget exhausted")?;
    selected
        .push(at)
        .map_err(|_| "action target limit exceeded")?;
    if scope == Scope::Selected {
        return Ok(selected);
    }
    let mut children =
        MemoryVec::new(budget, rows.len()).map_err(|_| "action memory budget exhausted")?;
    for _ in rows {
        children
            .push((None::<usize>, None::<usize>))
            .map_err(|_| "action memory budget exhausted")?;
    }
    for (index, row) in rows.iter().enumerate().rev() {
        if let Some(parent) = row
            .parent_pid
            .and_then(|pid| rows.binary_search_by_key(&pid, |row| row.key.pid).ok())
        {
            let previous = children.get(parent).ok_or("invalid topology")?.0;
            children.get_mut(index).ok_or("invalid topology")?.1 = previous;
            children.get_mut(parent).ok_or("invalid topology")?.0 = Some(index);
        }
    }
    let mut current = children.get(at).and_then(|links| links.0);
    while let Some(index) = current {
        if selected.contains(&index) {
            return Err("cycle in the selected subtree");
        }
        selected
            .push(index)
            .map_err(|_| "observed subtree exceeds 256 targets")?;
        if let Some(child) = children.get(index).and_then(|links| links.0) {
            current = Some(child);
            continue;
        }
        let mut cursor = index;
        loop {
            if cursor == at {
                current = None;
                break;
            }
            if let Some(next) = children.get(cursor).and_then(|links| links.1) {
                current = Some(next);
                break;
            }
            cursor = rows
                .get(cursor)
                .and_then(|row| row.parent_pid)
                .and_then(|pid| rows.binary_search_by_key(&pid, |row| row.key.pid).ok())
                .ok_or("inconsistent subtree ancestry")?;
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use crate::action_plan;
    use crate::{actions, budget, hierarchy};
    use hierarchy::{Input, ProcessKey};
    fn key(pid: u32) -> ProcessKey {
        ProcessKey {
            generation: 1,
            pid,
            start_ticks: 10,
        }
    }
    fn row(pid: u32, parent: u32) -> Input {
        Input {
            key: key(pid),
            parent_pid: Some(parent),
            cpu: None,
            rss: None,
        }
    }
    #[test]
    fn caller_view_and_protected_namespace_members_are_explicit() {
        let valid = b"Pid:\t42\nTgid:\t42\nUid:\t1000 1000 1000 1000\nNStgid:\t42\n";
        let status = action_plan::status(valid).unwrap();
        assert!(action_plan::caller(status, 42).is_ok());
        assert!(action_plan::caller(status, 43).is_err());
        assert!(action_plan::protected(status, 42).is_err());
        assert!(action_plan::protected(status, 43).is_ok());
        let nested = b"Pid: 42\nTgid: 42\nUid: 1000 1000 1000 1000\nNStgid: 42 1\n";
        let status = action_plan::status(nested).unwrap();
        assert!(action_plan::caller(status, 42).is_err());
        assert!(action_plan::protected(status, 43).is_err());
        for invalid in [
            b"Pid: 42\nTgid: 42\nUid: 1000 1000 1000 1000\n".as_slice(),
            b"Pid: 42\nTgid: 43\nUid: 1000 1000 1000 1000\nNStgid: 42\n",
            b"Pid: 42\nTgid: 42\nUid: 1000 1000 1000 1000\nNStgid: 43\n",
            b"Pid: 42\nTgid: 42\nUid: 1000\nNStgid: 42\n",
            b"Pid: 42\nPid: 42\nTgid: 42\nUid: 1000 1000 1000 1000\nNStgid: 42\n",
            b"Pid: 42\nTgid: 42\nUid: 1000 1000 1000 1000\nNStgid: 42\nNStgid: 42\n",
        ] {
            assert!(action_plan::status(invalid).is_err());
        }
    }
    #[test]
    fn subtree_is_deterministic_preorder_and_refuses_partial_bounds() {
        let budget = budget::Budget::new(budget::LIMIT).unwrap();
        let rows = [
            row(10, 0),
            row(11, 10),
            row(12, 10),
            row(13, 11),
            row(14, 0),
        ];
        let members =
            action_plan::members(&budget, &rows, key(10), actions::Scope::Descendants).unwrap();
        assert_eq!(&*members, &[0, 1, 3, 2]);
        let mut unrelated: Vec<_> = rows.to_vec();
        unrelated.extend([row(20, 21), row(21, 20)]);
        unrelated.extend((30..330).map(|pid| row(pid, pid - 1)));
        assert_eq!(
            &*action_plan::members(&budget, &unrelated, key(10), actions::Scope::Descendants)
                .unwrap(),
            &[0, 1, 3, 2]
        );
        let selected =
            action_plan::members(&budget, &rows, key(10), actions::Scope::Selected).unwrap();
        assert_eq!(&*selected, &[0]);
        let mut mismatch = key(10);
        mismatch.start_ticks = 11;
        assert!(
            action_plan::members(&budget, &rows, mismatch, actions::Scope::Descendants).is_err()
        );
        let maximum: Vec<_> = (1..=256)
            .map(|pid| row(pid, if pid == 1 { 0 } else { 1 }))
            .collect();
        assert_eq!(
            action_plan::members(&budget, &maximum, key(1), actions::Scope::Descendants)
                .unwrap()
                .len(),
            256
        );
        let overflow: Vec<_> = (1..=257)
            .map(|pid| row(pid, if pid == 1 { 0 } else { 1 }))
            .collect();
        assert!(
            action_plan::members(&budget, &overflow, key(1), actions::Scope::Descendants).is_err()
        );
        assert!(action_plan::members(
            &budget,
            &[row(1, 2), row(2, 1)],
            key(1),
            actions::Scope::Descendants
        )
        .is_err());
    }
}
