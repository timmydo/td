//! Borrow SQLite columns only while their statement lives; emit caller bytes.
use super::{format_error, sequence, sql};
use crate::{
    format::{scalar::Writer, Sequence, Table},
    ids::AccountId,
    ports,
};
use rusqlite::{params, types::ValueRef, Connection, Row};

struct Columns<'a, 'b> {
    row: &'a Row<'b>,
    position: usize,
}
impl<'a, 'b> Columns<'a, 'b> {
    fn new(row: &'a Row<'b>, position: usize) -> Self {
        Self { row, position }
    }
    fn take(&mut self) -> Result<ValueRef<'a>, ports::Error> {
        let index = self.position;
        self.position = self.position.checked_add(1).ok_or(ports::Error::Corrupt)?;
        self.row.get_ref(index).map_err(sql)
    }
    fn integer(&mut self) -> Result<i64, ports::Error> {
        match self.take()? {
            ValueRef::Integer(v) => Ok(v),
            _ => Err(ports::Error::Corrupt),
        }
    }
    fn u8(&mut self, w: &mut Writer<'_>) -> Result<(), ports::Error> {
        w.u8(u8::try_from(self.integer()?).map_err(|_| ports::Error::Corrupt)?)
            .map_err(format_error)
    }
    fn u32(&mut self, w: &mut Writer<'_>) -> Result<(), ports::Error> {
        w.u32(u32::try_from(self.integer()?).map_err(|_| ports::Error::Corrupt)?)
            .map_err(format_error)
    }
    fn i64(&mut self, w: &mut Writer<'_>) -> Result<(), ports::Error> {
        w.i64(self.integer()?).map_err(format_error)
    }
    fn blob(&mut self) -> Result<&'a [u8], ports::Error> {
        match self.take()? {
            ValueRef::Blob(v) => Ok(v),
            _ => Err(ports::Error::Corrupt),
        }
    }
    fn fixed(&mut self, w: &mut Writer<'_>, length: usize) -> Result<(), ports::Error> {
        let value = self.blob()?;
        if value.len() != length {
            return Err(ports::Error::Corrupt);
        }
        w.put(value).map_err(format_error)
    }
    fn text(&mut self, w: &mut Writer<'_>, maximum: usize) -> Result<(), ports::Error> {
        match self.take()? {
            ValueRef::Text(v) => {
                let v = std::str::from_utf8(v).map_err(|_| ports::Error::Corrupt)?;
                w.text(v, maximum).map_err(format_error)
            }
            _ => Err(ports::Error::Corrupt),
        }
    }
    fn optional_fixed(&mut self, w: &mut Writer<'_>, length: usize) -> Result<(), ports::Error> {
        match self.take()? {
            ValueRef::Null => w.boolean(false).map_err(format_error),
            ValueRef::Blob(v) if v.len() == length => {
                w.boolean(true).map_err(format_error)?;
                w.put(v).map_err(format_error)
            }
            _ => Err(ports::Error::Corrupt),
        }
    }
    fn optional_text(&mut self, w: &mut Writer<'_>, maximum: usize) -> Result<(), ports::Error> {
        match self.take()? {
            ValueRef::Null => w.boolean(false).map_err(format_error),
            ValueRef::Text(v) => {
                let v = std::str::from_utf8(v).map_err(|_| ports::Error::Corrupt)?;
                w.boolean(true).map_err(format_error)?;
                w.text(v, maximum).map_err(format_error)
            }
            _ => Err(ports::Error::Corrupt),
        }
    }
    fn optional_i64(&mut self, w: &mut Writer<'_>) -> Result<(), ports::Error> {
        match self.take()? {
            ValueRef::Null => w.boolean(false).map_err(format_error),
            ValueRef::Integer(v) => {
                w.boolean(true).map_err(format_error)?;
                w.i64(v).map_err(format_error)
            }
            _ => Err(ports::Error::Corrupt),
        }
    }
}
fn key_columns(table: Table) -> usize {
    match table {
        Table::Memberships | Table::Keywords | Table::ThreadAnchors | Table::Recipients => 2,
        Table::Imports => 4,
        _ => 1,
    }
}
pub(super) fn key(table: Table, row: &Row<'_>, output: &mut [u8]) -> Result<usize, ports::Error> {
    let mut columns = Columns::new(row, 0);
    let mut writer = Writer::new(output);
    match table {
        Table::Memberships => {
            columns.fixed(&mut writer, 16)?;
            columns.fixed(&mut writer, 16)?;
        }
        Table::Keywords => {
            columns.fixed(&mut writer, 16)?;
            let ValueRef::Text(keyword) = columns.take()? else {
                return Err(ports::Error::Corrupt);
            };
            writer.put(keyword).map_err(format_error)?;
        }
        Table::ThreadAnchors => {
            columns.text(&mut writer, 1004)?;
            columns.fixed(&mut writer, 16)?;
        }
        Table::Recipients => {
            columns.fixed(&mut writer, 16)?;
            writer
                .u32_key(u32::try_from(columns.integer()?).map_err(|_| ports::Error::Corrupt)?)
                .map_err(format_error)?;
        }
        Table::Imports => {
            columns.fixed(&mut writer, 16)?;
            columns.u8(&mut writer)?;
            writer.bytes(columns.blob()?, 998).map_err(format_error)?;
            writer.bytes(columns.blob()?, 998).map_err(format_error)?;
        }
        _ => columns.fixed(&mut writer, 16)?,
    }
    Ok(writer.written())
}
pub(super) fn value(
    db: &Connection,
    account: AccountId,
    table: Table,
    row: &Row<'_>,
    output: &mut [u8],
) -> Result<(usize, Sequence), ports::Error> {
    let mut columns = Columns::new(row, key_columns(table));
    let mut writer = Writer::new(output);
    match table {
        Table::Blobs => {
            columns.u8(&mut writer)?;
            writer
                .u64(u64::try_from(columns.integer()?).map_err(|_| ports::Error::Corrupt)?)
                .map_err(format_error)?;
            columns.fixed(&mut writer, 32)?;
            columns.i64(&mut writer)?;
        }
        Table::Mailboxes => {
            columns.text(&mut writer, 1024)?;
            columns.optional_fixed(&mut writer, 16)?;
            columns.optional_text(&mut writer, 64)?;
            columns.u32(&mut writer)?;
            columns.u8(&mut writer)?;
        }
        Table::Emails => {
            columns.fixed(&mut writer, 16)?;
            columns.fixed(&mut writer, 16)?;
            columns.i64(&mut writer)?;
            let origin = u8::try_from(columns.integer()?).map_err(|_| ports::Error::Corrupt)?;
            writer.u8(origin).map_err(format_error)?;
            if origin == 1 {
                let family = columns.integer()?;
                writer
                    .u8(u8::try_from(family).map_err(|_| ports::Error::Corrupt)?)
                    .map_err(format_error)?;
                columns.fixed(
                    &mut writer,
                    match family {
                        4 => 4,
                        6 => 16,
                        _ => return Err(ports::Error::Corrupt),
                    },
                )?;
                columns.optional_text(&mut writer, 64)?;
                columns.u8(&mut writer)?;
                columns.text(&mut writer, 255)?;
                columns.text(&mut writer, 254)?;
                let count = columns.integer()?;
                let email = match row.get_ref(0).map_err(sql)? {
                    ValueRef::Blob(v) => v,
                    _ => return Err(ports::Error::Corrupt),
                };
                receipt(db, account, email, count, &mut writer)?;
            } else {
                for _ in 0..7 {
                    if !matches!(columns.take()?, ValueRef::Null) {
                        return Err(ports::Error::Corrupt);
                    }
                }
            }
        }
        Table::Memberships | Table::Keywords | Table::Threads | Table::ThreadAnchors => {}
        Table::Submissions => {
            for _ in 0..4 {
                columns.fixed(&mut writer, 16)?;
            }
            columns.text(&mut writer, 254)?;
            columns.i64(&mut writer)?;
            columns.i64(&mut writer)?;
            columns.u32(&mut writer)?;
            columns.optional_i64(&mut writer)?;
            columns.u8(&mut writer)?;
            columns.optional_fixed(&mut writer, 16)?;
        }
        Table::Recipients => {
            columns.text(&mut writer, 254)?;
            columns.u8(&mut writer)?;
            columns.u8(&mut writer)?;
            columns.optional_fixed(&mut writer, 16)?;
            columns.u32(&mut writer)?;
            columns.optional_i64(&mut writer)?;
            columns.u8(&mut writer)?;
            columns.optional_i64(&mut writer)?;
            columns.optional_text(&mut writer, 4096)?;
            columns.optional_text(&mut writer, 4096)?;
            columns.u8(&mut writer)?;
            columns.text(&mut writer, 512)?;
        }
        Table::Leases => {
            columns.fixed(&mut writer, 16)?;
            columns.fixed(&mut writer, 16)?;
            columns.i64(&mut writer)?;
            columns.u8(&mut writer)?;
        }
        Table::Imports => {
            columns.fixed(&mut writer, 16)?;
            columns.optional_fixed(&mut writer, 16)?;
            columns.fixed(&mut writer, 32)?;
        }
    }
    let changed = sequence(columns.blob()?)?;
    if columns.position != row.as_ref().column_count() {
        return Err(ports::Error::Corrupt);
    }
    Ok((writer.written(), changed))
}
fn receipt(
    db: &Connection,
    account: AccountId,
    email: &[u8],
    count: i64,
    writer: &mut Writer<'_>,
) -> Result<(), ports::Error> {
    if !(1..=1000).contains(&count) {
        return Err(ports::Error::Corrupt);
    }
    let start = writer.written();
    writer
        .u32(u32::try_from(count).map_err(|_| ports::Error::Corrupt)?)
        .map_err(format_error)?;
    let mut statement=db.prepare("SELECT ordinal,address FROM smtp_receipt_recipients WHERE account=?1 AND email_id=?2 ORDER BY ordinal").map_err(sql)?;
    let mut rows = statement
        .query(params![account.as_bytes().as_slice(), email])
        .map_err(sql)?;
    for ordinal in 0..count {
        let row = rows.next().map_err(sql)?.ok_or(ports::Error::Corrupt)?;
        let mut columns = Columns::new(row, 0);
        if columns.integer()? != ordinal {
            return Err(ports::Error::Corrupt);
        }
        columns.text(writer, 254)?;
        if writer
            .written()
            .checked_sub(start)
            .ok_or(ports::Error::Corrupt)?
            > 32768
        {
            return Err(ports::Error::Corrupt);
        }
    }
    if rows.next().map_err(sql)?.is_some() {
        return Err(ports::Error::Corrupt);
    }
    Ok(())
}
