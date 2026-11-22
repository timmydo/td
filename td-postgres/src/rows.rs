//! A query's retained result: one buffer of values with their bounds,
//! charged against the caller's `result_bytes` as it grows.
use crate::types::{Decode, DecodeError};
use crate::wire::{DataRow, RowDescription};
use crate::{Error, Limits};

/// What a column costs beyond its name, and a value beyond its bytes.
const COLUMN_COST: usize = 16;
const CELL_COST: usize = 8;
/// A cell length that marks NULL.
const NULL: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
struct Column {
    name: (u32, u32),
    type_oid: u32,
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    start: u32,
    len: u32,
}

/// Every row a query returned.
#[derive(Debug, Default)]
pub struct Rows {
    columns: Vec<Column>,
    names: Vec<u8>,
    cells: Vec<Cell>,
    data: Vec<u8>,
    rows: usize,
    charged: usize,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The number of columns each row has.
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    pub fn column_name(&self, column: usize) -> Option<&str> {
        let column = self.columns.get(column)?;
        self.name(column)
    }

    pub fn column_type(&self, column: usize) -> Option<u32> {
        Some(self.columns.get(column)?.type_oid)
    }

    pub fn row(&self, index: usize) -> Option<RowRef<'_>> {
        (index < self.rows).then_some(RowRef { rows: self, index })
    }

    pub fn iter(&self) -> impl Iterator<Item = RowRef<'_>> {
        (0..self.rows).map(move |index| RowRef { rows: self, index })
    }

    fn name(&self, column: &Column) -> Option<&str> {
        let (start, len) = column.name;
        let start = usize::try_from(start).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        std::str::from_utf8(self.names.get(start..end)?).ok()
    }

    fn charge(&mut self, bytes: usize, limits: &Limits) -> Result<(), Error> {
        let charged = self.charged.checked_add(bytes).ok_or(Error::Limit)?;
        if charged > limits.result_bytes {
            return Err(Error::Limit);
        }
        self.charged = charged;
        Ok(())
    }

    /// Take the result's columns. Every one must be binary.
    pub(crate) fn describe(
        &mut self,
        description: &RowDescription<'_>,
        limits: &Limits,
    ) -> Result<(), Error> {
        for field in description.fields() {
            if field.format_code != 1 {
                return Err(Error::Protocol);
            }
            self.charge(field.name.len().saturating_add(COLUMN_COST), limits)?;
            let start = u32::try_from(self.names.len()).map_err(|_| Error::Limit)?;
            let len = u32::try_from(field.name.len()).map_err(|_| Error::Limit)?;
            td_alloc::extend_from_slice(&mut self.names, field.name)?;
            td_alloc::push(
                &mut self.columns,
                Column {
                    name: (start, len),
                    type_oid: field.type_oid,
                },
            )?;
        }
        if self.columns.len() != description.len() {
            return Err(Error::Protocol);
        }
        Ok(())
    }

    /// Keep one row, whose width the caller has checked.
    pub(crate) fn keep(&mut self, row: &DataRow<'_>, limits: &Limits) -> Result<(), Error> {
        let before = self.cells.len();
        for value in row.values() {
            let len = value.map_or(0, <[u8]>::len);
            self.charge(len.saturating_add(CELL_COST), limits)?;
            let start = u32::try_from(self.data.len()).map_err(|_| Error::Limit)?;
            let cell = match value {
                Some(bytes) => {
                    td_alloc::extend_from_slice(&mut self.data, bytes)?;
                    Cell {
                        start,
                        len: u32::try_from(bytes.len())
                            .ok()
                            .filter(|&len| len != NULL)
                            .ok_or(Error::Limit)?,
                    }
                }
                None => Cell { start, len: NULL },
            };
            td_alloc::push(&mut self.cells, cell)?;
        }
        if self.cells.len().saturating_sub(before) != self.columns.len() {
            return Err(Error::Protocol);
        }
        self.rows += 1;
        Ok(())
    }

    fn raw(&self, index: usize, column: usize) -> Result<(u32, Option<&[u8]>), DecodeError> {
        let width = self.columns.len();
        let type_oid = self
            .columns
            .get(column)
            .ok_or(DecodeError::Column)?
            .type_oid;
        let at = index
            .checked_mul(width)
            .and_then(|at| at.checked_add(column))
            .ok_or(DecodeError::Column)?;
        let cell = self.cells.get(at).ok_or(DecodeError::Column)?;
        if cell.len == NULL {
            return Ok((type_oid, None));
        }
        let start = usize::try_from(cell.start).map_err(|_| DecodeError::Invalid)?;
        let end = start
            .checked_add(usize::try_from(cell.len).map_err(|_| DecodeError::Invalid)?)
            .ok_or(DecodeError::Invalid)?;
        let bytes = self.data.get(start..end).ok_or(DecodeError::Invalid)?;
        Ok((type_oid, Some(bytes)))
    }

    fn position(&self, name: &str) -> Result<usize, DecodeError> {
        self.columns
            .iter()
            .position(|column| self.name(column) == Some(name))
            .ok_or(DecodeError::Column)
    }
}

/// One row of a `Rows`.
#[derive(Clone, Copy, Debug)]
pub struct RowRef<'a> {
    rows: &'a Rows,
    index: usize,
}

impl<'a> RowRef<'a> {
    /// The number of columns.
    pub fn len(&self) -> usize {
        self.rows.width()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.width() == 0
    }

    /// Column `column`'s value as `T`.
    pub fn get<T: Decode<'a>>(&self, column: usize) -> Result<T, DecodeError> {
        let (type_oid, raw) = self.rows.raw(self.index, column)?;
        T::decode(type_oid, raw)
    }

    /// The first column named `name`, as `T`.
    pub fn get_named<T: Decode<'a>>(&self, name: &str) -> Result<T, DecodeError> {
        self.get(self.rows.position(name)?)
    }

    /// Column `column`'s binary bytes, `None` for NULL, whatever its type.
    pub fn raw(&self, column: usize) -> Result<Option<&'a [u8]>, DecodeError> {
        Ok(self.rows.raw(self.index, column)?.1)
    }
}

/// The one row `Client::query_one` and `query_opt` return.
#[derive(Debug)]
pub struct Row(pub(crate) Rows);

impl Row {
    pub fn row(&self) -> RowRef<'_> {
        RowRef {
            rows: &self.0,
            index: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.0.width()
    }

    pub fn is_empty(&self) -> bool {
        self.0.width() == 0
    }

    pub fn get<'a, T: Decode<'a>>(&'a self, column: usize) -> Result<T, DecodeError> {
        self.row().get(column)
    }

    pub fn get_named<'a, T: Decode<'a>>(&'a self, name: &str) -> Result<T, DecodeError> {
        self.row().get_named(name)
    }

    pub fn column_name(&self, column: usize) -> Option<&str> {
        self.0.column_name(column)
    }
}
