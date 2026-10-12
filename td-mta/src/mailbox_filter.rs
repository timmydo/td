//! Passive Mailbox conditions. Boolean composition and retained query I/O are separate.
use crate::{
    format::row::{MailboxRow, MAX_MAILBOX_NAME, MAX_ROLE},
    ids::MailboxId,
    wire,
};

pub const MAX_TEXT_BYTES: usize = 4096;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    UnsupportedFilter,
    Scratch,
    Unicode,
    InvalidRow,
    Invariant,
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "mailbox filtering: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Compiled lowercase scalars and KMP fallback lengths in caller-owned scratch.
/// This neither normalizes text nor interprets query phrases or escapes.
#[derive(Debug)]
pub struct NamePattern<'a> {
    scalars: &'a [char],
    fallback: &'a [usize],
}
impl<'a> NamePattern<'a> {
    /// Exact entry count after the fixed input byte ceiling is checked.
    pub fn scratch_len(value: &str) -> Result<usize, Error> {
        if value.len() > MAX_TEXT_BYTES {
            return Err(Error::UnsupportedFilter);
        }
        Ok(value.chars().count())
    }
    /// The caller charges compilation work and supplies admitted scratch.
    /// At most MAX_TEXT_BYTES entries in each buffer are needed. An error may
    /// modify scratch but never returns a partially compiled pattern.
    pub fn compile(
        value: &str,
        scalars: &'a mut [char],
        fallback: &'a mut [usize],
    ) -> Result<Self, Error> {
        let count = Self::scratch_len(value)?;
        let scalars = scalars.get_mut(..count).ok_or(Error::Scratch)?;
        let fallback = fallback.get_mut(..count).ok_or(Error::Scratch)?;
        for (slot, scalar) in scalars.iter_mut().zip(value.chars()) {
            *slot = td_mime::unicode::simple_lowercase(scalar).map_err(|_| Error::Unicode)?;
        }
        let mut matched = 0;
        for index in 0..count {
            if index != 0 {
                let scalar = scalars.get(index).ok_or(Error::Invariant)?;
                while matched != 0 && scalars.get(matched) != Some(scalar) {
                    matched = *fallback.get(matched - 1).ok_or(Error::Invariant)?;
                }
                if scalars.get(matched) == Some(scalar) {
                    matched += 1;
                }
            }
            *fallback.get_mut(index).ok_or(Error::Invariant)? = matched;
        }
        Ok(Self { scalars, fallback })
    }
    /// One bounded row comparison, with linear scalar matching and no allocation.
    /// The caller charges work and checks its original deadline around this call.
    pub fn matches(&self, name: &str) -> Result<bool, Error> {
        if name.len() > MAX_MAILBOX_NAME {
            return Err(Error::InvalidRow);
        }
        if self.scalars.is_empty() {
            return Ok(true);
        }
        if self.scalars.len() > MAX_MAILBOX_NAME {
            return Ok(false);
        }
        let mut matched = 0;
        for value in name.chars() {
            let value = td_mime::unicode::simple_lowercase(value).map_err(|_| Error::Unicode)?;
            while matched != 0 && self.scalars.get(matched) != Some(&value) {
                matched = *self.fallback.get(matched - 1).ok_or(Error::Invariant)?;
            }
            if self.scalars.get(matched) == Some(&value) {
                matched += 1;
                if matched == self.scalars.len() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub enum Parent {
    #[default]
    Any,
    Root,
    Local(MailboxId),
    NoLocal,
}
impl Parent {
    /// A syntactically valid foreign or wrong-kind ID cannot match a local parent.
    /// Explicit null uses Root; omission uses Any.
    pub fn exact(value: wire::Id<'_>) -> Self {
        value.local().map(Self::Local).unwrap_or(Self::NoLocal)
    }
    fn matches(self, value: Option<MailboxId>) -> bool {
        match self {
            Self::Any => true,
            Self::Root => value.is_none(),
            Self::Local(id) => value == Some(id),
            Self::NoLocal => false,
        }
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub enum Role<'a> {
    #[default]
    Any,
    Exact(Option<&'a str>),
}
/// All specified fields are conjunctive. JSON shape checking, text-operand
/// admission and operator composition remain the method decoder's responsibility.
#[derive(Debug, Default)]
pub struct Condition<'a> {
    pub parent: Parent,
    pub name: Option<&'a NamePattern<'a>>,
    pub role: Role<'a>,
    pub has_any_role: Option<bool>,
    pub subscribed: Option<bool>,
}
impl Condition<'_> {
    /// No normalization, account authorization, view pin or full row validation.
    pub fn matches(&self, row: MailboxRow<'_>) -> Result<bool, Error> {
        if row.name.len() > MAX_MAILBOX_NAME || row.role.is_some_and(|role| role.len() > MAX_ROLE) {
            return Err(Error::InvalidRow);
        }
        if !self.parent.matches(row.parent)
            || matches!(self.role, Role::Exact(role) if role != row.role)
            || self
                .has_any_role
                .is_some_and(|role| role != row.role.is_some())
            || self.subscribed.is_some_and(|value| value != row.subscribed)
        {
            return Ok(false);
        }
        match self.name {
            Some(name) => name.matches(row.name),
            None => Ok(true),
        }
    }
}

#[cfg(test)]
#[path = "mailbox_filter_tests.rs"]
mod tests;
