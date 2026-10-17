//! Canonical PostgreSQL change-row decoding shared by reads and audits.
use crate::{
    format::{ObjectType, Sequence},
    ports::{Change, ChangeAction, ChangeCursor, ChangeRecord, Error},
};
use tokio_postgres::Row;

pub(crate) fn read(row: &Row) -> Result<ChangeRecord, Error> {
    decode(
        row.try_get(0).map_err(|_| Error::Corrupt)?,
        row.try_get(1).map_err(|_| Error::Corrupt)?,
        row.try_get(2).map_err(|_| Error::Corrupt)?,
        row.try_get(3).map_err(|_| Error::Corrupt)?,
        row.try_get(4).map_err(|_| Error::Corrupt)?,
    )
}
fn decode(
    sequence: &[u8],
    operation: i64,
    action: i64,
    id: &[u8],
    kind: i64,
) -> Result<ChangeRecord, Error> {
    let sequence = u64::from_be_bytes(sequence.try_into().map_err(|_| Error::Corrupt)?);
    if sequence == 0 {
        return Err(Error::Corrupt);
    }
    let operation = u32::try_from(operation)
        .ok()
        .filter(|value| *value < 4096)
        .ok_or(Error::Corrupt)?;
    let kind = ObjectType::from_tag(u16::try_from(kind).map_err(|_| Error::Corrupt)?)
        .map_err(|_| Error::Corrupt)?;
    if kind == ObjectType::Identity {
        return Err(Error::Corrupt);
    }
    let action = match action {
        1 => ChangeAction::Created,
        2 => ChangeAction::Updated,
        3 => ChangeAction::Destroyed,
        _ => return Err(Error::Corrupt),
    };
    Ok(ChangeRecord {
        cursor: ChangeCursor {
            sequence: Sequence::from_u64(sequence),
            operation,
        },
        change: Change {
            kind,
            id: id.try_into().map_err(|_| Error::Corrupt)?,
            action,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn change_fields_preserve_unsigned_sequence_and_refuse_invalid_scalars() -> Result<(), Error> {
        for sequence in [1, u64::MAX] {
            for operation in [0, 4095] {
                for kind in [1, 2, 3, 5] {
                    for action in [1, 2, 3] {
                        let row =
                            decode(&sequence.to_be_bytes(), operation, action, &[7; 16], kind)?;
                        assert_eq!(row.cursor.sequence.number(), sequence);
                        assert_eq!(i64::from(row.cursor.operation), operation);
                        assert_eq!(i64::from(row.change.kind.tag()), kind);
                        assert_eq!(row.change.id, [7; 16]);
                    }
                }
            }
        }
        for sequence in [&[][..], &[0; 7], &[0; 8], &[0; 9]] {
            assert_eq!(decode(sequence, 0, 1, &[7; 16], 1), Err(Error::Corrupt));
        }
        for operation in [-1, 4096, i64::MAX] {
            assert_eq!(
                decode(&1u64.to_be_bytes(), operation, 1, &[7; 16], 1),
                Err(Error::Corrupt)
            );
        }
        for kind in [-1, 0, 4, 6, i64::MAX] {
            assert_eq!(
                decode(&1u64.to_be_bytes(), 0, 1, &[7; 16], kind),
                Err(Error::Corrupt)
            );
        }
        for action in [-1, 0, 4, i64::MAX] {
            assert_eq!(
                decode(&1u64.to_be_bytes(), 0, action, &[7; 16], 1),
                Err(Error::Corrupt)
            );
        }
        for id in [&[][..], &[7; 15], &[7; 17]] {
            assert_eq!(
                decode(&1u64.to_be_bytes(), 0, 1, id, 1),
                Err(Error::Corrupt)
            );
        }
        Ok(())
    }
}
