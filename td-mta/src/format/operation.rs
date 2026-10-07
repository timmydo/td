//! Borrowed PUT/DELETE/CHANGE bytes. SQLite owns transaction integrity; domain policy is separate.
use super::{
    key::Key,
    row,
    scalar::{Reader, Writer},
    Error, ObjectType, Table, MAX_KEY_BYTES, MAX_VALUE_BYTES, MIN_KEY_BYTES,
    OPERATION_HEADER_BYTES,
};
use crate::ports::{Change, ChangeAction, Mutation, OperationKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Value<'a> {
    Row(Mutation<'a>),
    Change(Change),
}

struct Prefix {
    kind: OperationKind,
    tag: u16,
    key: usize,
    value: usize,
}
impl Prefix {
    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes);
        let opcode = reader.u8()?;
        let action = reader.u8()?;
        let tag = reader.u16()?;
        let key = usize::try_from(reader.u32()?).map_err(|_| Error::Overflow)?;
        let value = usize::try_from(reader.u32()?).map_err(|_| Error::Overflow)?;
        reader.finish()?;
        let kind = match (opcode, action) {
            (1, 0) => OperationKind::Put,
            (2, 0) => OperationKind::Delete,
            (3, 1) => OperationKind::Change(ChangeAction::Created),
            (3, 2) => OperationKind::Change(ChangeAction::Updated),
            (3, 3) => OperationKind::Change(ChangeAction::Destroyed),
            _ => return Err(Error::InvalidTag),
        };
        match kind {
            OperationKind::Put => {
                Table::from_tag(tag)?;
            }
            OperationKind::Delete => {
                Table::from_tag(tag)?;
                if value != 0 {
                    return Err(Error::InvalidValue);
                }
            }
            OperationKind::Change(_) => {
                ObjectType::from_tag(tag)?;
                if key != 16 || value != 0 {
                    return Err(Error::InvalidValue);
                }
            }
        }
        length(key, value)?;
        Ok(Self {
            kind,
            tag,
            key,
            value,
        })
    }
}
fn length(key: usize, value: usize) -> Result<usize, Error> {
    if !(MIN_KEY_BYTES..=MAX_KEY_BYTES).contains(&key) || value > MAX_VALUE_BYTES {
        return Err(Error::Limit);
    }
    OPERATION_HEADER_BYTES
        .checked_add(key)
        .and_then(|n| n.checked_add(value))
        .ok_or(Error::Overflow)
}
/// Bound an operation from its exact 12-byte prefix. This supplies no integrity or row proof.
pub fn extent(prefix: &[u8]) -> Result<usize, Error> {
    let prefix = Prefix::decode(prefix)?;
    length(prefix.key, prefix.value)
}

/// Locally valid bytes, including the known Identity CHANGE tag. V1 transaction
/// validation must reject Identity changes and validate final rows and references.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Operation<'a> {
    decoded: Value<'a>,
    key: &'a [u8],
    value: &'a [u8],
}
impl<'a> Operation<'a> {
    pub fn put(table: Table, key: &'a [u8], value: &'a [u8]) -> Result<Self, Error> {
        length(key.len(), value.len())?;
        let (typed_key, row) = row::decode_record(table, key, value)?;
        Ok(Self {
            decoded: Value::Row(Mutation::Put {
                key: typed_key,
                row,
            }),
            key,
            value,
        })
    }
    pub fn delete(table: Table, key: &'a [u8]) -> Result<Self, Error> {
        length(key.len(), 0)?;
        let typed_key = Key::decode(table, key)?;
        typed_key.validate_local()?;
        Ok(Self {
            decoded: Value::Row(Mutation::Delete(typed_key)),
            key,
            value: &[],
        })
    }
    pub fn change(kind: ObjectType, action: ChangeAction, id: &'a [u8; 16]) -> Self {
        Self {
            decoded: Value::Change(Change {
                kind,
                id: *id,
                action,
            }),
            key: id,
            value: &[],
        }
    }
    pub const fn value(self) -> Value<'a> {
        self.decoded
    }
    pub const fn key_bytes(self) -> &'a [u8] {
        self.key
    }
    pub const fn value_bytes(self) -> &'a [u8] {
        self.value
    }
    pub fn encoded_len(self) -> Result<usize, Error> {
        length(self.key.len(), self.value.len())
    }
    pub const fn kind(self) -> OperationKind {
        match self.decoded {
            Value::Row(Mutation::Put { .. }) => OperationKind::Put,
            Value::Row(Mutation::Delete(_)) => OperationKind::Delete,
            Value::Change(change) => OperationKind::Change(change.action),
        }
    }
    pub const fn type_tag(self) -> u16 {
        match self.decoded {
            Value::Row(Mutation::Put { key, .. } | Mutation::Delete(key)) => key.table().tag(),
            Value::Change(change) => change.kind.tag() as u16,
        }
    }
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Error> {
        let prefix = Prefix::decode(
            bytes
                .get(..OPERATION_HEADER_BYTES)
                .ok_or(Error::Truncated)?,
        )?;
        let mut reader = Reader::new(
            bytes
                .get(OPERATION_HEADER_BYTES..)
                .ok_or(Error::Truncated)?,
        );
        let key = reader.take(prefix.key)?;
        let value = reader.take(prefix.value)?;
        reader.finish()?;
        match prefix.kind {
            OperationKind::Put => Self::put(Table::from_tag(prefix.tag)?, key, value),
            OperationKind::Delete => Self::delete(Table::from_tag(prefix.tag)?, key),
            OperationKind::Change(action) => Ok(Self::change(
                ObjectType::from_tag(prefix.tag)?,
                action,
                key.try_into().map_err(|_| Error::InvalidValue)?,
            )),
        }
    }
    /// All returned errors preserve output; success preserves its suffix.
    pub fn encode(self, output: &mut [u8]) -> Result<usize, Error> {
        let size = self.encoded_len()?;
        let key_len = u32::try_from(self.key.len()).map_err(|_| Error::Overflow)?;
        let value_len = u32::try_from(self.value.len()).map_err(|_| Error::Overflow)?;
        let mut writer = Writer::new(output.get_mut(..size).ok_or(Error::OutputFull)?);
        let (opcode, action) = match self.kind() {
            OperationKind::Put => (1, 0),
            OperationKind::Delete => (2, 0),
            OperationKind::Change(ChangeAction::Created) => (3, 1),
            OperationKind::Change(ChangeAction::Updated) => (3, 2),
            OperationKind::Change(ChangeAction::Destroyed) => (3, 3),
        };
        writer.u8(opcode)?;
        writer.u8(action)?;
        writer.u16(self.type_tag())?;
        writer.u32(key_len)?;
        writer.u32(value_len)?;
        writer.put(self.key)?;
        writer.put(self.value)?;
        Ok(writer.written())
    }
}
