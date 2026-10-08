//! Borrowed operation sources share one transactional implementation.
use super::*;
use crate::format::batch::Batch;
use crate::ports::OperationKind;

#[derive(Clone, Copy)]
pub(super) enum Operations<'o, 'i> {
    Typed(&'o [Operation<'i>]),
    Encoded(&'o Batch<'i, 'o>),
}
impl<'o, 'i> Operations<'o, 'i> {
    pub(super) fn len(self) -> usize {
        match self {
            Self::Typed(operations) => operations.len(),
            Self::Encoded(batch) => batch.len(),
        }
    }
    pub(super) fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub(super) fn get(self, index: usize) -> Result<Operation<'i>, ports::Error> {
        match self {
            Self::Typed(operations) => operations.get(index).copied().ok_or(ports::Error::Invalid),
            Self::Encoded(batch) => batch
                .get(index)
                .map_err(|_| ports::Error::Invalid)?
                .ok_or(ports::Error::Invalid),
        }
    }
    fn key(
        self,
        index: usize,
        tables: &[Table],
        put_only: bool,
    ) -> Result<Option<Key<'i>>, ports::Error> {
        match self {
            Self::Typed(operations) => {
                let operation = operations
                    .get(index)
                    .copied()
                    .ok_or(ports::Error::Invalid)?;
                match operation.value() {
                    Value::Row(Mutation::Put { key, .. }) if tables.contains(&key.table()) => {
                        Ok(Some(key))
                    }
                    Value::Row(Mutation::Delete(key))
                        if !put_only && tables.contains(&key.table()) =>
                    {
                        Ok(Some(key))
                    }
                    _ => Ok(None),
                }
            }
            Self::Encoded(batch) => {
                let entry = batch.descriptor(index).ok_or(ports::Error::Invalid)?;
                if matches!(entry.kind, OperationKind::Change(_))
                    || (put_only && entry.kind != OperationKind::Put)
                    || !tables.iter().any(|table| table.tag() == entry.type_tag)
                {
                    return Ok(None);
                }
                batch.row_key(index).map_err(|_| ports::Error::Invalid)
            }
        }
    }
    pub(super) fn blob_put(self, index: usize) -> Result<Option<BlobId>, ports::Error> {
        Ok(match self.key(index, &[Table::Blobs], true)? {
            Some(Key::Blob(id)) => Some(id),
            _ => None,
        })
    }
    pub(super) fn submission(
        self,
        index: usize,
    ) -> Result<Option<crate::ids::SubmissionId>, ports::Error> {
        Ok(
            match self.key(index, &[Table::Submissions, Table::Recipients], false)? {
                Some(Key::Submission(id) | Key::Recipient(id, _)) => Some(id),
                _ => None,
            },
        )
    }
}
