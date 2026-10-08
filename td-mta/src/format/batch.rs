//! Complete bounded operation framing bound to its original immutable bytes.
use super::{
    operation::{self, Operation},
    Error, MAX_TRANSACTION_BYTES, MAX_TRANSACTION_OPERATIONS, OPERATION_HEADER_BYTES,
};
use crate::ports::{StagedOperation, TransactionInput};

/// Locally validated input, not account authorization or a commit capability.
/// The borrowed slots cannot change while this binding is live.
/// ```compile_fail,E0506
/// use td_mta::{format::batch::Batch, ports::TransactionInput};
/// let bytes = [0; 28];
/// let mut slots = [None];
/// let batch = Batch::decode(TransactionInput { bytes: &bytes, count: 1 }, &mut slots)?;
/// slots[0] = None;
/// let _ = batch.get(0);
/// # Ok::<(), td_mta::format::Error>(())
/// ```
/// ```compile_fail,E0506
/// use td_mta::{format::batch::Batch, ports::TransactionInput};
/// let mut bytes = [0; 28];
/// let mut slots = [None];
/// let batch = Batch::decode(TransactionInput { bytes: &bytes, count: 1 }, &mut slots)?;
/// bytes[0] = 1;
/// let _ = batch.get(0);
/// # Ok::<(), td_mta::format::Error>(())
/// ```
pub struct Batch<'i, 's> {
    bytes: &'i [u8],
    slots: &'s [Option<StagedOperation>],
}
impl<'i, 's> Batch<'i, 's> {
    /// Decode the entire batch into caller-reserved offset slots without allocation.
    /// On failure, written slots are provisional; no complete binding is returned.
    pub fn decode(
        input: TransactionInput<'i>,
        slots: &'s mut [Option<StagedOperation>],
    ) -> Result<Self, Error> {
        if input.count == 0 {
            return Err(Error::InvalidValue);
        }
        if input.count > MAX_TRANSACTION_OPERATIONS || input.bytes.len() > MAX_TRANSACTION_BYTES {
            return Err(Error::Limit);
        }
        let slots = slots.get_mut(..input.count).ok_or(Error::OutputFull)?;
        let mut offset = 0usize;
        for (ordinal, slot) in slots.iter_mut().enumerate() {
            let remaining = input.bytes.get(offset..).ok_or(Error::Truncated)?;
            let prefix = remaining
                .get(..OPERATION_HEADER_BYTES)
                .ok_or(Error::Truncated)?;
            let length = operation::extent(prefix)?;
            let bytes = remaining.get(..length).ok_or(Error::Truncated)?;
            // Bound the slice first, then reuse complete operation validation.
            let operation = Operation::decode(bytes)?;
            let key_offset = offset
                .checked_add(OPERATION_HEADER_BYTES)
                .ok_or(Error::Overflow)?;
            let value_offset = key_offset
                .checked_add(operation.key_bytes().len())
                .ok_or(Error::Overflow)?;
            offset = offset.checked_add(length).ok_or(Error::Overflow)?;
            *slot = Some(StagedOperation {
                kind: operation.kind(),
                type_tag: operation.type_tag(),
                key_offset: u32::try_from(key_offset).map_err(|_| Error::Overflow)?,
                key_len: u32::try_from(operation.key_bytes().len()).map_err(|_| Error::Overflow)?,
                value_offset: u32::try_from(value_offset).map_err(|_| Error::Overflow)?,
                value_len: u32::try_from(operation.value_bytes().len())
                    .map_err(|_| Error::Overflow)?,
                ordinal: u32::try_from(ordinal).map_err(|_| Error::Overflow)?,
            });
        }
        if offset != input.bytes.len() {
            return Err(Error::TrailingBytes);
        }
        Ok(Self {
            bytes: input.bytes,
            slots,
        })
    }
    /// Passive checked offsets; copies confer no source or commit authority.
    pub fn descriptor(&self, ordinal: usize) -> Option<StagedOperation> {
        self.slots.get(ordinal).copied().flatten()
    }
    /// Reborrow a mutation key without decoding its row value. CHANGE has no key.
    pub fn row_key(&self, ordinal: usize) -> Result<Option<super::key::Key<'i>>, Error> {
        let Some(slot) = self.descriptor(ordinal) else {
            return Ok(None);
        };
        if matches!(slot.kind, crate::ports::OperationKind::Change(_)) {
            return Ok(None);
        }
        let start = usize::try_from(slot.key_offset).map_err(|_| Error::Overflow)?;
        let end = start
            .checked_add(usize::try_from(slot.key_len).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let bytes = self.bytes.get(start..end).ok_or(Error::Truncated)?;
        super::key::Key::decode(super::Table::from_tag(slot.type_tag)?, bytes).map(Some)
    }
    pub fn len(&self) -> usize {
        self.slots.len()
    }
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
    /// Reborrow one locally checked operation from the original bytes.
    /// Decoding again spends caller work; this does not renew admission.
    pub fn get(&self, ordinal: usize) -> Result<Option<Operation<'i>>, Error> {
        let Some(slot) = self.slots.get(ordinal) else {
            return Ok(None);
        };
        let slot = slot.ok_or(Error::InvalidValue)?;
        let start = usize::try_from(slot.key_offset)
            .map_err(|_| Error::Overflow)?
            .checked_sub(OPERATION_HEADER_BYTES)
            .ok_or(Error::Overflow)?;
        let end = usize::try_from(slot.value_offset)
            .map_err(|_| Error::Overflow)?
            .checked_add(usize::try_from(slot.value_len).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let bytes = self.bytes.get(start..end).ok_or(Error::Truncated)?;
        Operation::decode(bytes).map(Some)
    }
}
