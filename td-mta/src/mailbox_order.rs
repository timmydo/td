//! Bounded flat/sibling Mailbox ordering; tree traversal and query I/O are separate.
use crate::{format::row::MAX_MAILBOX_NAME, ids::MailboxId};
use std::cmp::Ordering;
pub const MAX_COMPARATORS: usize = 8;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    UnsupportedSort,
    InvalidKey,
    Unicode,
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "mailbox ordering: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug)]
enum Property {
    SortOrder,
    Name(Collation),
}
#[derive(Clone, Copy, Debug)]
enum Collation {
    Unicode,
    Octet,
    AsciiCasemap,
}
#[derive(Clone, Copy, Debug)]
pub struct Comparator {
    property: Property,
    ascending: bool,
}
impl Comparator {
    /// JSON argument shape checking is the caller's responsibility. Numeric
    /// comparisons ignore even unknown string collations, as RFC 8620 requires.
    pub fn new(property: &str, ascending: bool, collation: Option<&str>) -> Result<Self, Error> {
        let property = match property {
            "sortOrder" => Property::SortOrder,
            "name" => Property::Name(match collation {
                None => Collation::Unicode,
                Some("i;octet") => Collation::Octet,
                Some("i;ascii-casemap") => Collation::AsciiCasemap,
                _ => return Err(Error::UnsupportedSort),
            }),
            _ => return Err(Error::UnsupportedSort),
        };
        Ok(Self {
            property,
            ascending,
        })
    }
}
const DEFAULT: &[Comparator] = &[
    Comparator {
        property: Property::SortOrder,
        ascending: true,
    },
    Comparator {
        property: Property::Name(Collation::Unicode),
        ascending: true,
    },
];
/// Borrowed bounded comparison fields, not a validated storage row or authority.
#[derive(Clone, Copy, Debug)]
pub struct Key<'a> {
    id: MailboxId,
    name: &'a str,
    sort_order: u32,
}
impl<'a> Key<'a> {
    pub fn new(id: MailboxId, name: &'a str, sort_order: u32) -> Result<Self, Error> {
        if name.len() > MAX_MAILBOX_NAME || sort_order >= 1 << 31 {
            return Err(Error::InvalidKey);
        }
        Ok(Self {
            id,
            name,
            sort_order,
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Plan<'a> {
    comparators: &'a [Comparator],
}
impl<'a> Plan<'a> {
    pub fn new(comparators: &'a [Comparator]) -> Result<Self, Error> {
        if comparators.len() > MAX_COMPARATORS {
            return Err(Error::UnsupportedSort);
        }
        Ok(Self {
            comparators: if comparators.is_empty() {
                DEFAULT
            } else {
                comparators
            },
        })
    }
    /// One bounded CPU comparison. The caller charges work and checks the
    /// original deadline around comparisons and owns any sort storage.
    /// At most 2 * MAX_COMPARATORS * MAX_MAILBOX_NAME scalar lookups occur.
    /// The sort must propagate errors; never substitute an ordering on failure.
    pub fn compare(self, left: Key<'_>, right: Key<'_>) -> Result<Ordering, Error> {
        for comparator in self.comparators {
            let order = match comparator.property {
                Property::SortOrder => left.sort_order.cmp(&right.sort_order),
                Property::Name(collation) => names(left.name, right.name, collation)?,
            };
            if order != Ordering::Equal {
                return Ok(if comparator.ascending {
                    order
                } else {
                    order.reverse()
                });
            }
        }
        Ok(left.id.cmp(&right.id))
    }
}
fn names(left: &str, right: &str, collation: Collation) -> Result<Ordering, Error> {
    match collation {
        Collation::Octet => Ok(left.as_bytes().cmp(right.as_bytes())),
        // RFC 4790 maps a-z to A-Z, which matters around punctuation.
        Collation::AsciiCasemap => Ok(left
            .bytes()
            .map(|b| b.to_ascii_uppercase())
            .cmp(right.bytes().map(|b| b.to_ascii_uppercase()))),
        Collation::Unicode => {
            let (mut left, mut right) = (left.chars(), right.chars());
            loop {
                let order = match (left.next(), right.next()) {
                    (Some(left), Some(right)) => td_mime::unicode::simple_lowercase(left)
                        .map_err(|_| Error::Unicode)?
                        .cmp(
                            &td_mime::unicode::simple_lowercase(right)
                                .map_err(|_| Error::Unicode)?,
                        ),
                    (None, None) => return Ok(Ordering::Equal),
                    (None, Some(_)) => return Ok(Ordering::Less),
                    (Some(_), None) => return Ok(Ordering::Greater),
                };
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
        }
    }
}
#[cfg(test)]
#[path = "mailbox_order_tests.rs"]
mod tests;
