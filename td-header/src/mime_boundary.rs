//! Pure recognition of one logical MIME delimiter line; admission is external.
pub const MAX_BOUNDARY_BYTES: usize = 70;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Match {
    pub closing: bool,
    pub ignored_suffix: bool,
}
/// Fixed prefix/suffix state. Exclude the accepted line ending before feed.
/// This matcher neither validates boundary grammar nor authorizes part extents.
#[derive(Clone, Copy, Debug)]
pub struct Line<'a> {
    boundary: &'a [u8],
    prefix: u8,
    mismatch: bool,
    suffix_started: bool,
    hyphen: bool,
    closing: bool,
    ignored_suffix: bool,
}
impl<'a> Line<'a> {
    pub fn new(boundary: &'a [u8]) -> Option<Self> {
        if !(1..=MAX_BOUNDARY_BYTES).contains(&boundary.len()) {
            return None;
        }
        Some(Self {
            boundary,
            prefix: 0,
            mismatch: false,
            suffix_started: false,
            hyphen: false,
            closing: false,
            ignored_suffix: false,
        })
    }
    pub const fn boundary(&self) -> &'a [u8] {
        self.boundary
    }
    pub fn reset(&mut self) {
        self.prefix = 0;
        self.mismatch = false;
        self.suffix_started = false;
        self.hyphen = false;
        self.closing = false;
        self.ignored_suffix = false;
    }
    /// Fund one bounded transition and at most one boundary-byte comparison.
    pub fn feed(&mut self, byte: u8) {
        if self.mismatch {
            return;
        }
        let prefix = usize::from(self.prefix);
        if prefix < self.boundary.len() + 2 {
            let wanted = if prefix < 2 {
                Some(b'-')
            } else {
                self.boundary.get(prefix - 2).copied()
            };
            self.mismatch = wanted != Some(byte);
            self.prefix = self.prefix.saturating_add(1);
            return;
        }
        if !self.suffix_started {
            self.suffix_started = true;
            if byte == b'-' {
                self.hyphen = true;
                return;
            }
        } else if self.hyphen {
            self.hyphen = false;
            if byte == b'-' {
                self.closing = true;
                return;
            }
            self.ignored_suffix = true;
        }
        self.ignored_suffix |= !matches!(byte, b' ' | b'\t');
    }
    /// A matching prefix is classified only at this complete logical line EOF.
    pub fn finish(&self) -> Option<Match> {
        if self.mismatch || usize::from(self.prefix) != self.boundary.len() + 2 {
            return None;
        }
        Some(Match {
            closing: self.closing,
            ignored_suffix: self.ignored_suffix || self.hyphen,
        })
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn prefixes_closes_padding_and_other_suffixes() {
        for (source, expected) in [
            (b"--abc".as_slice(), Some((false, false))),
            (b"--abc \t", Some((false, false))),
            (b"--abc--", Some((true, false))),
            (b"--abc-- \t", Some((true, false))),
            (b"--abc-", Some((false, true))),
            (b"--abc- ", Some((false, true))),
            (b"--abc---", Some((true, true))),
            (b"--abcdef", Some((false, true))),
            (b"--abc--other", Some((true, true))),
            (b"--abc\r", Some((false, true))),
            (b"--ab", None),
            (b" --abc", None),
            (b"--abx--abc", None),
        ] {
            let mut line = Line::new(b"abc").unwrap();
            for byte in source {
                line.feed(*byte);
            }
            assert_eq!(
                line.finish().map(|m| (m.closing, m.ignored_suffix)),
                expected
            );
        }
        assert!(Line::new(b"").is_none());
        assert!(Line::new(&[b'a'; 71]).is_none());
        const {
            assert!(std::mem::size_of::<Line<'_>>() <= 24);
        }
        let mut line = Line::new(b"a-").unwrap();
        for byte in b"--a--" {
            line.feed(*byte);
        }
        assert_eq!(
            line.finish(),
            Some(Match {
                closing: false,
                ignored_suffix: true
            })
        );
        line.reset();
        assert_eq!(line.boundary(), b"a-");
        assert!(line.finish().is_none());
        for byte in b"--a---" {
            line.feed(*byte);
        }
        assert_eq!(
            line.finish(),
            Some(Match {
                closing: true,
                ignored_suffix: false
            })
        );
    }
    #[test]
    fn every_suffix_octet_has_deterministic_prefix_recovery() {
        for byte in 0..=u8::MAX {
            let mut line = Line::new(b"b").unwrap();
            for prefix in b"--b" {
                line.feed(*prefix);
            }
            line.feed(byte);
            let found = line.finish().unwrap();
            assert!(!found.closing);
            assert_eq!(found.ignored_suffix, !matches!(byte, b' ' | b'\t'));
        }
    }
}
