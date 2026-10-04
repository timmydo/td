//! Checked passive views of resident bytes; admission and identity are external.
use std::ops::Range;

/// Map an absolute half-open extent into one caller-supplied resident window.
/// No octet is inspected or copied. A view grants no source/publication authority;
/// callers retain exact source identity and fund any later reads before access.
/// This checks the selected extent, not the complete source's absolute endpoint.
/// ```no_run
/// assert_eq!(td_header::resident::slice(b"abcdef", 17, 19..22), Some(b"cde".as_slice()));
/// ```
#[inline]
#[must_use]
pub fn slice(source: &[u8], base: u64, extent: Range<u64>) -> Option<&[u8]> {
    let start = usize::try_from(extent.start.checked_sub(base)?).ok()?;
    let end = usize::try_from(extent.end.checked_sub(base)?).ok()?;
    source.get(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonzero_window_edges_and_original_borrow() {
        let source = b"abcdef";
        assert_eq!(slice(source, 17, 17..23), Some(source.as_slice()));
        assert_eq!(slice(source, 17, 19..22), Some(b"cde".as_slice()));
        assert_eq!(slice(source, 17, 17..17), Some(b"".as_slice()));
        assert_eq!(slice(source, 17, 23..23), Some(b"".as_slice()));
        assert_eq!(slice(source, 17, 19..19), Some(b"".as_slice()));
        let view = slice(source, 17, 19..22);
        assert!(view.is_some_and(|view| view.as_ptr() == source.as_ptr().wrapping_add(2)));
        assert_eq!(slice(b"", 17, 17..17), Some(b"".as_slice()));
    }

    #[test]
    fn invalid_and_large_absolute_extents_are_checked() {
        for (start, end) in [
            (16, 17),
            (17, 16),
            (22, 21),
            (23, 24),
            (24, 24),
            (17, u64::MAX),
        ] {
            assert_eq!(slice(b"abcdef", 17, start..end), None);
        }
        let base = u64::MAX - 6;
        assert_eq!(
            slice(b"abcdef", base, base..u64::MAX),
            Some(b"abcdef".as_slice())
        );
        assert_eq!(
            slice(b"abcdef", base, u64::MAX..u64::MAX),
            Some(b"".as_slice())
        );
        assert_eq!(slice(b"abcdef", base, 0..u64::MAX), None);
        let base = u64::MAX - 1;
        assert_eq!(slice(b"abc", base, base..u64::MAX), Some(b"a".as_slice()));
    }
}
