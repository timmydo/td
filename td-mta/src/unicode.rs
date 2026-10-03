//! Fixed Unicode 17 lookups. Decomposition is recursive, without reordering.
#[path = "unicode_tables.rs"]
mod tables;
use std::cmp::Ordering;

/// A compiled table violated its checked generation contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidTable;
impl std::fmt::Display for InvalidTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid compiled Unicode table")
    }
}
impl std::error::Error for InvalidTable {}

/// One to four recursively decomposed scalars, still in source order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decomposition {
    values: [char; 4],
    len: u8,
}
impl Decomposition {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = char> + '_ {
        self.values.iter().copied().take(usize::from(self.len))
    }
}
fn sequence(values: impl IntoIterator<Item = u32>) -> Result<Decomposition, InvalidTable> {
    let mut result = Decomposition {
        values: ['\0'; 4],
        len: 0,
    };
    for value in values {
        let slot = result
            .values
            .get_mut(usize::from(result.len))
            .ok_or(InvalidTable)?;
        *slot = char::from_u32(value).ok_or(InvalidTable)?;
        result.len += 1;
    }
    if result.len == 0 {
        return Err(InvalidTable);
    }
    Ok(result)
}

/// No work meter is changed; the enclosing cursor charges this bounded lookup.
pub fn decompose(value: char) -> Result<Decomposition, InvalidTable> {
    let code = u32::from(value);
    if (0xac00..=0xd7a3).contains(&code) {
        let index = code - 0xac00;
        let values = [
            0x1100 + index / 588,
            0x1161 + (index % 588) / 28,
            0x11a7 + index % 28,
        ];
        return sequence(values.into_iter().take(if index % 28 == 0 { 2 } else { 3 }));
    }
    match tables::DECOMPOSITION.binary_search_by_key(&code, |row| row.0) {
        Err(_) => sequence([code]),
        Ok(index) => {
            let &(_, start, len) = tables::DECOMPOSITION.get(index).ok_or(InvalidTable)?;
            let start = usize::from(start);
            let end = start.checked_add(usize::from(len)).ok_or(InvalidTable)?;
            sequence(
                tables::DECOMPOSED
                    .get(start..end)
                    .ok_or(InvalidTable)?
                    .iter()
                    .copied(),
            )
        }
    }
}

pub fn combining_class(value: char) -> Result<u8, InvalidTable> {
    let code = u32::from(value);
    match tables::CLASSES.binary_search_by(|&(start, end, _)| {
        if code < start {
            Ordering::Greater
        } else if code > end {
            Ordering::Less
        } else {
            Ordering::Equal
        }
    }) {
        Err(_) => Ok(0),
        Ok(index) => tables::CLASSES
            .get(index)
            .map(|row| row.2)
            .ok_or(InvalidTable),
    }
}

/// Return a canonical pair composition; the caller enforces blocking/order.
pub fn compose(left: char, right: char) -> Result<Option<char>, InvalidTable> {
    let left = u32::from(left);
    let right = u32::from(right);
    let hangul = if (0x1100..=0x1112).contains(&left) && (0x1161..=0x1175).contains(&right) {
        Some(0xac00 + (left - 0x1100) * 588 + (right - 0x1161) * 28)
    } else if (0xac00..=0xd7a3).contains(&left)
        && (left - 0xac00) % 28 == 0
        && (0x11a8..=0x11c2).contains(&right)
    {
        Some(left + right - 0x11a7)
    } else {
        None
    };
    if let Some(code) = hangul {
        return char::from_u32(code).map(Some).ok_or(InvalidTable);
    }
    match tables::COMPOSITION.binary_search_by_key(&(left, right), |row| (row.0, row.1)) {
        Err(_) => Ok(None),
        Ok(index) => {
            let row = tables::COMPOSITION.get(index).ok_or(InvalidTable)?;
            char::from_u32(row.2).map(Some).ok_or(InvalidTable)
        }
    }
}

/// Simple lowercase, not full folding, contextual casing or normalization.
pub fn simple_lowercase(value: char) -> Result<char, InvalidTable> {
    match tables::LOWERCASE.binary_search_by_key(&u32::from(value), |row| row.0) {
        Err(_) => Ok(value),
        Ok(index) => {
            let row = tables::LOWERCASE.get(index).ok_or(InvalidTable)?;
            char::from_u32(row.1).ok_or(InvalidTable)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    #[test]
    fn every_table_entry_and_class_boundary_is_reachable() {
        assert!(std::mem::size_of::<Decomposition>() <= 20);
        for &(code, offset, len) in tables::DECOMPOSITION {
            let value = char::from_u32(code).unwrap();
            let expected = tables::DECOMPOSED
                .get(usize::from(offset)..usize::from(offset) + usize::from(len))
                .unwrap();
            assert!(decompose(value)
                .unwrap()
                .iter()
                .map(u32::from)
                .eq(expected.iter().copied()));
        }
        for &(left, right, value) in tables::COMPOSITION {
            assert_eq!(
                compose(
                    char::from_u32(left).unwrap(),
                    char::from_u32(right).unwrap()
                )
                .unwrap(),
                char::from_u32(value)
            );
        }
        for &(code, lower) in tables::LOWERCASE {
            assert_eq!(
                simple_lowercase(char::from_u32(code).unwrap()).unwrap(),
                char::from_u32(lower).unwrap()
            );
        }
        for &(start, end, class) in tables::CLASSES {
            for code in start..=end {
                assert_eq!(
                    combining_class(char::from_u32(code).unwrap()).unwrap(),
                    class
                );
            }
            for edge in [start.checked_sub(1), end.checked_add(1)]
                .into_iter()
                .flatten()
                .filter_map(char::from_u32)
            {
                let expected = tables::CLASSES
                    .iter()
                    .find(|&&(a, b, _)| (a..=b).contains(&u32::from(edge)))
                    .map_or(0, |row| row.2);
                assert_eq!(combining_class(edge).unwrap(), expected);
            }
        }
    }
    #[test]
    fn pinned_examples_identity_exclusions_and_hangul_edges() {
        for (value, expected) in [
            ('é', "e\u{301}"),
            ('\u{212b}', "A\u{30a}"),
            ('\u{1fa}', "A\u{30a}\u{301}"),
            ('\u{ac00}', "\u{1100}\u{1161}"),
            ('\u{d7a3}', "\u{1112}\u{1175}\u{11c2}"),
        ] {
            assert!(decompose(value).unwrap().iter().eq(expected.chars()));
        }
        for value in [
            '\0',
            '\u{378}',
            '\u{fdd0}',
            '\u{10ffff}',
            '\u{fb01}',
            '\u{ac00}',
            '\u{abff}',
            '\u{d7a4}',
        ] {
            assert_eq!(simple_lowercase(value).unwrap(), value);
            assert_eq!(combining_class(value).unwrap(), 0);
            if value != '\u{ac00}' {
                assert!(decompose(value).unwrap().iter().eq([value]));
            }
        }
        assert_eq!(simple_lowercase('\u{130}').unwrap(), 'i');
        assert_eq!(simple_lowercase('\u{10400}').unwrap(), '\u{10428}');
        assert_eq!(compose('\u{0b47}', '\u{0b3e}').unwrap(), Some('\u{0b4b}'));
        assert_eq!(compose('\u{0915}', '\u{093c}').unwrap(), None);
        for (left, right) in [
            ('\u{10ff}', '\u{1161}'),
            ('\u{1113}', '\u{1161}'),
            ('\u{1100}', '\u{1160}'),
            ('\u{1100}', '\u{1176}'),
            ('\u{ac00}', '\u{11a7}'),
            ('\u{ac00}', '\u{11c3}'),
            ('\u{ac01}', '\u{11a8}'),
            ('\u{abff}', '\u{11a8}'),
            ('\u{d7a4}', '\u{11a8}'),
        ] {
            assert_eq!(compose(left, right).unwrap(), None);
        }
        assert!(sequence([]).is_err());
        assert!(sequence([0xd800]).is_err());
        assert!(sequence([0x110000]).is_err());
        assert!(sequence([0x41; 5]).is_err());
    }
}
