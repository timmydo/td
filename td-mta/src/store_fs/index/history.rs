//! Bounded retirement and reclamation of native account change history.
use super::*;

const MAX_ROWS: u32 = 4096;
const PREFIX: &str = "SELECT sequence,operation FROM changes WHERE account=?1 AND sequence<=?2 ORDER BY sequence,operation LIMIT ?3";
const DELETE_PREFIX: &str =
    "DELETE FROM changes WHERE account=?1 AND (sequence,operation)<=(?2,?3)";

/// Caller-authorized retention boundary and bounded physical reclamation work.
#[derive(Clone, Copy, Debug)]
pub struct HistoryPruneRequest {
    pub account: AccountId,
    pub expected: Sequence,
    pub through: Sequence,
    pub max_rows: u32,
    pub deadline: Deadline,
}

/// Passive committed receipt; it owns neither a snapshot nor admission credit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryPruned {
    pub identity: ViewIdentity,
    pub removed: u32,
    pub more: bool,
}

impl IndexStore<'_> {
    /// Atomically retire complete sequences and reclaim at most max_rows entries.
    /// The caller supplies retention policy, account authority and admission.
    pub fn prune_history(
        &self,
        request: HistoryPruneRequest,
    ) -> Result<HistoryPruned, CommitError> {
        self.transaction(request.deadline, |native, _| {
            if request.max_rows == 0 || request.max_rows > MAX_ROWS {
                return Err(ports::Error::Invalid);
            }
            reserve_wal(self.root)?;
            native.run(|db| db.execute_batch("BEGIN IMMEDIATE").map_err(sql))?;
            let current = native.run(|db| identity(db, request.account, self.epoch))?;
            if current.committed_sequence != request.expected
                || request.through < current.history_floor
            {
                return Err(ports::Error::Conflict);
            }
            if request.through > current.committed_sequence {
                return Err(ports::Error::Invalid);
            }
            let (removed, more, last) = native.run(|db| {
                let mut statement = db.prepare(PREFIX).map_err(sql)?;
                let mut rows = statement
                    .query(params![
                        request.account.as_bytes().as_slice(),
                        request.through.number().to_be_bytes().as_slice(),
                        i64::from(request.max_rows) + 1,
                    ])
                    .map_err(sql)?;
                let mut removed = 0;
                let mut last = None;
                let mut more = false;
                while let Some(row) = rows.next().map_err(sql)? {
                    let sequence = fixed_blob::<8>(row, 0)?;
                    let operation: u32 = row.get(1).map_err(sql)?;
                    if u64::from(operation) >= MAX_OPERATIONS as u64 {
                        return Err(ports::Error::Corrupt);
                    }
                    if removed == request.max_rows {
                        more = true;
                        break;
                    }
                    removed += 1;
                    last = Some((sequence, operation));
                }
                Ok((removed, more, last))
            })?;
            native.run(|db| {
                if let Some((sequence, operation)) = last {
                    let deleted = db
                        .execute(
                            DELETE_PREFIX,
                            params![
                                request.account.as_bytes().as_slice(),
                                sequence.as_slice(),
                                i64::from(operation),
                            ],
                        )
                        .map_err(sql)?;
                    if deleted != removed as usize {
                        return Err(ports::Error::Corrupt);
                    }
                }
                let updated = db
                    .execute(
                        "UPDATE accounts SET floor=?2 WHERE id=?1",
                        params![
                            request.account.as_bytes().as_slice(),
                            request.through.number().to_be_bytes().as_slice(),
                        ],
                    )
                    .map_err(sql)?;
                if updated != 1 {
                    return Err(ports::Error::Corrupt);
                }
                Ok(())
            })?;
            Ok(HistoryPruned {
                identity: ViewIdentity {
                    history_floor: request.through,
                    ..current
                },
                removed,
                more,
            })
        })
    }
}

#[cfg(test)]
#[path = "pruning_tests.rs"]
mod tests;
