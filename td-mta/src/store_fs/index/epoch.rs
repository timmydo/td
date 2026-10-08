//! Exclusive epoch replacement before a restored snapshot enters service.
use super::*;

impl IndexStore<'_> {
    /// Replace the store epoch using one fill from an admitted entropy source.
    /// The caller owns restore authorization, verification and service quiescence.
    /// Every failure consumes this owner; retain the root and reopen to inspect it.
    /// Native entropy and connection cleanup are not deadline-interruptible.
    ///
    /// ```compile_fail,E0505
    /// use td_mta::{ids::AccountId, ports::{Deadline, Entropy}, store_fs::IndexStore};
    /// fn live(store: IndexStore<'_>, entropy: &mut dyn Entropy,
    ///         account: AccountId, deadline: Deadline) {
    ///     let view = store.view(account, deadline).unwrap();
    ///     let _ = store.renew_epoch(entropy, deadline);
    ///     drop(view);
    /// }
    /// ```
    ///
    /// ```compile_fail,E0505
    /// use td_mta::{ids::{AccountId, BlobId}, ports::{Deadline, Entropy}, store_fs::IndexStore};
    /// fn body_live(store: IndexStore<'_>, entropy: &mut dyn Entropy,
    ///              account: AccountId, blob: BlobId, length: u64, deadline: Deadline) {
    ///     let mut view = store.view(account, deadline).unwrap();
    ///     let input = view.open_blob_input(&td_crypto::Provider, blob, length).unwrap();
    ///     let body = input.finish().unwrap();
    ///     let _ = store.renew_epoch(entropy, deadline);
    ///     drop(body);
    /// }
    /// ```
    pub fn renew_epoch(
        mut self,
        entropy: &mut dyn ports::Entropy,
        deadline: Deadline,
    ) -> Result<Self, CommitError> {
        let epoch = self.transaction(deadline, |native, _| {
            if lock(&self.readers)?
                .iter()
                .any(|slot| matches!(slot, ReaderSlot::Borrowed))
            {
                return Err(ports::Error::Busy);
            }
            reserve_wal(self.root)?;
            native.check()?;
            let mut bytes = [0; 16];
            let filled = entropy.fill(&mut bytes);
            native.check()?;
            filled?;
            let epoch = StoreEpoch::from_bytes(bytes);
            if epoch == self.epoch {
                return Err(ports::Error::Conflict);
            }
            native.run(|db| db.execute_batch("BEGIN IMMEDIATE").map_err(sql))?;
            native.run(|db| {
                let changed = db
                    .execute(
                        "UPDATE store SET epoch=?1 WHERE id=1 AND epoch=?2",
                        params![
                            epoch.as_bytes().as_slice(),
                            self.epoch.as_bytes().as_slice()
                        ],
                    )
                    .map_err(sql)?;
                if changed != 1 {
                    return Err(ports::Error::Corrupt);
                }
                Ok(())
            })?;
            Ok(epoch)
        })?;
        self.epoch = epoch;
        Ok(self)
    }
}

#[cfg(test)]
#[path = "epoch_tests.rs"]
mod tests;
