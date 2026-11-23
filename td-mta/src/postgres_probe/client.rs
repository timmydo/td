//! One worker's PostgreSQL session: td-postgres's client under the
//! worker's deadline, its failures sorted into connection faults (the
//! worker's own error) and statement errors (the operation's), and its
//! rows read in place or copied through td-alloc.
use super::{wire::Wire, Error};
use std::{
    cell::{Cell, RefCell},
    time::Instant,
};
use td_postgres::{Array, Decode, RowRef, Severity};
pub(crate) use td_postgres::{CommitError, DecodeError, Encode, Error as DbError, SqlState};

/// The relay's wake channel: an empty NOTIFY on it is the only hint read.
const RELAY_CHANNEL: &str = "td_mta_relay";

/// Operations borrow the session shared, as the SQL helpers always have;
/// each call takes the client for its own duration and never re-enters.
pub(crate) struct Client {
    session: RefCell<td_postgres::Client<Wire>>,
    deadline: Cell<Instant>,
    fault: Cell<Option<Error>>,
    relay: Cell<bool>,
    dropped: Cell<u64>,
    pub(super) runtime_schema: Option<crate::postgres_schema::Receipt>,
}

impl Client {
    pub(super) fn new(session: td_postgres::Client<Wire>, deadline: Instant) -> Self {
        Self {
            session: RefCell::new(session),
            deadline: Cell::new(deadline),
            fault: Cell::new(None),
            relay: Cell::new(false),
            dropped: Cell::new(0),
            runtime_schema: None,
        }
    }

    pub(crate) fn inspect_schema(
        &self,
    ) -> Result<crate::postgres_schema::Receipt, crate::postgres_schema::Error> {
        match self.runtime_schema {
            Some(receipt) => Ok(receipt),
            None => crate::postgres_schema::lock_and_inspect(self),
        }
    }

    pub(super) fn set_deadline(&self, deadline: Instant) {
        self.deadline.set(deadline);
    }

    /// The first failure that ended the connection or crossed its
    /// deadline. Once set, no call reaches the server again.
    pub(super) fn fault(&self) -> Option<Error> {
        self.fault.get()
    }

    /// Whether a relay hint arrived since the last call, which clears it.
    pub(super) fn take_relay(&self) -> bool {
        self.relay.take()
    }

    fn record(&self, error: DbError, broken: bool) {
        let fault = match error {
            DbError::Deadline => Error::Deadline,
            // A statement's own refusal leaves a usable session.
            DbError::Db(db) if db.severity() == Severity::Error && !broken => return,
            DbError::RowCount | DbError::Invalid if !broken => return,
            _ => Error::Database,
        };
        if self.fault.get().is_none() {
            self.fault.set(Some(fault));
        }
    }

    /// Notifications are hints: keep only whether the relay's arrived, or
    /// any was dropped unread.
    fn take_notifications(&self, session: &mut td_postgres::Client<Wire>) {
        while let Some(notification) = session.take_notification() {
            if notification.channel() == RELAY_CHANNEL && notification.payload().is_empty() {
                self.relay.set(true);
            }
        }
        let dropped = session.dropped_notifications();
        if dropped != self.dropped.get() {
            self.dropped.set(dropped);
            self.relay.set(true);
        }
    }

    fn call<T>(
        &self,
        operation: impl FnOnce(&mut td_postgres::Client<Wire>, Instant) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        if self.fault.get().is_some() {
            return Err(DbError::Broken);
        }
        let mut session = self
            .session
            .try_borrow_mut()
            .map_err(|_| DbError::Invalid)?;
        let result = operation(&mut session, self.deadline.get());
        if let Err(error) = &result {
            self.record(*error, session.is_broken());
        }
        self.take_notifications(&mut session);
        result
    }

    /// The simple query: several statements, rows discarded. COMMIT and
    /// ROLLBACK go through `commit` and `rollback`.
    pub(crate) fn batch(&self, sql: &str) -> Result<(), DbError> {
        self.call(|session, deadline| session.batch(sql, deadline))
    }

    pub(crate) fn query(&self, sql: &str, parameters: &[&dyn Encode]) -> Result<Rows, DbError> {
        self.call(|session, deadline| session.query(sql, parameters, deadline))
            .map(Rows)
    }

    /// `query` for exactly one row; any other count is `RowCount`, which
    /// leaves the session usable.
    pub(crate) fn query_one(
        &self,
        sql: &str,
        parameters: &[&dyn Encode],
    ) -> Result<OneRow, DbError> {
        self.query_opt(sql, parameters)?.ok_or(DbError::RowCount)
    }

    /// `query` for at most one row; more is `RowCount`.
    pub(crate) fn query_opt(
        &self,
        sql: &str,
        parameters: &[&dyn Encode],
    ) -> Result<Option<OneRow>, DbError> {
        let rows = self.call(|session, deadline| session.query(sql, parameters, deadline))?;
        match rows.len() {
            0 => Ok(None),
            1 => Ok(Some(OneRow(rows))),
            _ => Err(DbError::RowCount),
        }
    }

    pub(crate) fn execute(&self, sql: &str, parameters: &[&dyn Encode]) -> Result<u64, DbError> {
        self.call(|session, deadline| session.execute(sql, parameters, deadline))
    }

    pub(crate) fn rollback(&self) -> Result<(), DbError> {
        self.call(|session, deadline| session.rollback(deadline))
    }

    /// COMMIT, with td-postgres's outcome. A COMMIT whose answer is lost
    /// is also a connection fault, so the worker reports the deadline or
    /// failure that lost it.
    pub(crate) fn commit(&self) -> Result<(), CommitError> {
        if self.fault.get().is_some() {
            return Err(CommitError::NotSent(DbError::Broken));
        }
        let mut session = self
            .session
            .try_borrow_mut()
            .map_err(|_| CommitError::NotSent(DbError::Invalid))?;
        let result = session.commit(self.deadline.get());
        let broken = session.is_broken();
        match result {
            Ok(()) => {}
            Err(CommitError::NotSent(error) | CommitError::OutcomeUnknown(error)) => {
                self.record(error, broken);
            }
            Err(CommitError::RolledBack(_)) if broken => self.record(DbError::Broken, true),
            Err(CommitError::RolledBack(_)) => {}
        }
        self.take_notifications(&mut session);
        result
    }

    /// Wait for a notification on the idle connection until `until`.
    pub(super) fn wait(&self, until: Instant) -> Result<(), DbError> {
        self.call(|session, _| session.wait(until))
    }

    /// Send Terminate by `until`; true once it is written.
    pub(super) fn close(self: Box<Self>, until: Instant) -> bool {
        self.session.into_inner().close(until).is_ok()
    }

    /// Read what the idle connection holds without a query: a server that
    /// ended it, a notice or a notification. With `until` past, nothing
    /// waits.
    pub(super) fn drain(&self, until: Instant) -> Result<(), DbError> {
        self.call(|session, _| {
            // `wait` returns at each held notification; take them and
            // read on until nothing more is ready.
            loop {
                session.wait(until)?;
                let mut any = false;
                while let Some(notification) = session.take_notification() {
                    any = true;
                    if notification.channel() == RELAY_CHANNEL && notification.payload().is_empty()
                    {
                        self.relay.set(true);
                    }
                }
                if !any {
                    return Ok(());
                }
            }
        })
    }
}

/// Every row of one query.
pub(crate) struct Rows(td_postgres::Rows);

impl Rows {
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn get(&self, index: usize) -> Option<Row<'_>> {
        (index < self.0.len()).then_some(Row {
            rows: &self.0,
            index,
        })
    }

    pub(crate) fn first(&self) -> Option<Row<'_>> {
        self.get(0)
    }

    pub(crate) fn iter(&self) -> RowIter<'_> {
        RowIter {
            rows: &self.0,
            front: 0,
            back: self.0.len(),
        }
    }

    /// The rows from `start` up to `end`.
    pub(crate) fn range(&self, start: usize, end: usize) -> Option<RowIter<'_>> {
        (start <= end && end <= self.0.len()).then_some(RowIter {
            rows: &self.0,
            front: start,
            back: end,
        })
    }
}

impl<'a> IntoIterator for &'a Rows {
    type Item = Row<'a>;
    type IntoIter = RowIter<'a>;

    fn into_iter(self) -> RowIter<'a> {
        self.iter()
    }
}

pub(crate) struct RowIter<'a> {
    rows: &'a td_postgres::Rows,
    front: usize,
    back: usize,
}

impl<'a> RowIter<'a> {
    /// The next row, without taking it.
    pub(crate) fn peek(&self) -> Option<Row<'a>> {
        (self.front < self.back).then_some(Row {
            rows: self.rows,
            index: self.front,
        })
    }
}

impl<'a> Iterator for RowIter<'a> {
    type Item = Row<'a>;

    fn next(&mut self) -> Option<Row<'a>> {
        if self.front >= self.back {
            return None;
        }
        let index = self.front;
        self.front += 1;
        Some(Row {
            rows: self.rows,
            index,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.back.saturating_sub(self.front);
        (left, Some(left))
    }
}

impl DoubleEndedIterator for RowIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        self.back -= 1;
        Some(Row {
            rows: self.rows,
            index: self.back,
        })
    }
}

impl ExactSizeIterator for RowIter<'_> {}

/// One row of a `Rows`.
#[derive(Clone, Copy)]
pub(crate) struct Row<'a> {
    rows: &'a td_postgres::Rows,
    index: usize,
}

impl<'a> Row<'a> {
    fn cell(&self) -> Result<RowRef<'a>, ColumnError> {
        self.rows
            .row(self.index)
            .ok_or(ColumnError::Decode(DecodeError::Column))
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.width()
    }

    pub(crate) fn try_get<I: Column, T: FromColumn<'a>>(&self, index: I) -> Result<T, ColumnError> {
        let rows = self.rows;
        let column = index.locate(rows.width(), |at| rows.column_name(at))?;
        T::from_column(self.cell()?, column)
    }
}

/// The one row of `query_one` or `query_opt`.
pub(crate) struct OneRow(td_postgres::Rows);

impl OneRow {
    pub(crate) fn row(&self) -> Row<'_> {
        Row {
            rows: &self.0,
            index: 0,
        }
    }

    pub(crate) fn try_get<'a, I: Column, T: FromColumn<'a>>(
        &'a self,
        index: I,
    ) -> Result<T, ColumnError> {
        self.row().try_get(index)
    }
}

/// Why a column could not be read: td-postgres's decode error, or a copy
/// td-alloc refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ColumnError {
    Decode(DecodeError),
    Allocation,
}

impl From<DecodeError> for ColumnError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl From<td_alloc::Error> for ColumnError {
    fn from(_: td_alloc::Error) -> Self {
        Self::Allocation
    }
}

/// A column named by position or by name, the first so named.
pub(crate) trait Column {
    fn locate<'n>(
        &self,
        width: usize,
        name: impl Fn(usize) -> Option<&'n str>,
    ) -> Result<usize, ColumnError>;
}

impl Column for usize {
    fn locate<'n>(
        &self,
        width: usize,
        _: impl Fn(usize) -> Option<&'n str>,
    ) -> Result<usize, ColumnError> {
        if *self < width {
            Ok(*self)
        } else {
            Err(ColumnError::Decode(DecodeError::Column))
        }
    }
}

impl Column for &str {
    fn locate<'n>(
        &self,
        width: usize,
        name: impl Fn(usize) -> Option<&'n str>,
    ) -> Result<usize, ColumnError> {
        (0..width)
            .find(|&at| name(at) == Some(*self))
            .ok_or(ColumnError::Decode(DecodeError::Column))
    }
}

/// A Rust value read from one column: td-postgres's in-place forms, or
/// owned copies through td-alloc.
pub(crate) trait FromColumn<'a>: Sized {
    fn from_column(row: RowRef<'a>, column: usize) -> Result<Self, ColumnError>;
}

macro_rules! in_place {
    ($($type:ty),*) => {$(
        impl<'a> FromColumn<'a> for $type {
            fn from_column(row: RowRef<'a>, column: usize) -> Result<Self, ColumnError> {
                Ok(row.get::<$type>(column)?)
            }
        }
    )*};
}

in_place!(bool, i16, i32, i64, &'a str, &'a [u8], Array<'a>);

impl<'a, T: FromColumn<'a>> FromColumn<'a> for Option<T> {
    fn from_column(row: RowRef<'a>, column: usize) -> Result<Self, ColumnError> {
        match row.raw(column)? {
            None => Ok(None),
            Some(_) => T::from_column(row, column).map(Some),
        }
    }
}

impl<'a> FromColumn<'a> for String {
    fn from_column(row: RowRef<'a>, column: usize) -> Result<Self, ColumnError> {
        Ok(td_alloc::string(row.get::<&str>(column)?)?)
    }
}

impl<'a> FromColumn<'a> for Vec<u8> {
    fn from_column(row: RowRef<'a>, column: usize) -> Result<Self, ColumnError> {
        Ok(td_alloc::copy(row.get::<&[u8]>(column)?)?)
    }
}

/// Each element of a one-dimensional array, copied.
fn elements<'a, E: Decode<'a>, T>(
    row: RowRef<'a>,
    column: usize,
    copy: impl Fn(E) -> Result<T, ColumnError>,
) -> Result<Vec<T>, ColumnError> {
    let array = row.get::<Array<'a>>(column)?;
    let mut out = td_alloc::with_capacity(array.len())?;
    for element in array.iter::<E>() {
        td_alloc::push(&mut out, copy(element?)?)?;
    }
    Ok(out)
}

impl<'a> FromColumn<'a> for Vec<Vec<u8>> {
    fn from_column(row: RowRef<'a>, column: usize) -> Result<Self, ColumnError> {
        elements(row, column, |bytes: &[u8]| Ok(td_alloc::copy(bytes)?))
    }
}

impl<'a> FromColumn<'a> for Vec<String> {
    fn from_column(row: RowRef<'a>, column: usize) -> Result<Self, ColumnError> {
        elements(row, column, |text: &str| Ok(td_alloc::string(text)?))
    }
}

impl<'a> FromColumn<'a> for Vec<i64> {
    fn from_column(row: RowRef<'a>, column: usize) -> Result<Self, ColumnError> {
        elements(row, column, |value: i64| Ok(value))
    }
}
