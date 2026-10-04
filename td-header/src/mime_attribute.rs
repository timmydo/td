//! RFC 2231 suffix spelling; no value assembly or candidate selection.
pub use crate::delimited::Extent;
use crate::{mime_token_octet, Charge, Work};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Form {
    Ordinary,
    Extended,
    Section { index: u64, encoded: bool },
    Malformed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Name {
    pub base: Extent,
    pub form: Form,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete(Name),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Work(E),
    InvalidState,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(error) => write!(f, "MIME attribute work: {error}"),
            Self::InvalidState => f.write_str("invalid MIME attribute state"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}
#[derive(Clone, Copy)]
enum Suffix {
    Base,
    Start,
    Digits,
    End,
    Bad,
}
/// A complete parameter name; no field or value authorization is granted.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_header::mime_attribute::Cursor<'_, ()>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {}
/// copied::<td_header::mime_attribute::Cursor<'_, ()>>();
/// ```
pub struct Cursor<'a, E: Copy> {
    source: &'a [u8],
    position: usize,
    base_end: Option<usize>,
    base_valid: bool,
    token_valid: bool,
    suffix: Suffix,
    index: u64,
    result: Option<Name>,
    failure: Option<Error<E>>,
}
impl<'a, E: Copy> Cursor<'a, E> {
    #[must_use]
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            position: 0,
            base_end: None,
            base_valid: true,
            token_valid: true,
            suffix: Suffix::Base,
            index: 0,
            result: None,
            failure: None,
        }
    }
    pub fn poll(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if let Some(name) = self.result {
            return Ok(Status::Complete(name));
        }
        let result = self.advance(work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    /// Explicit fresh admission, including after cached completion.
    /// The caller binds this zero-count callback to its live admission policy.
    pub fn check_work(&mut self, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = work.charge(Charge::default()).map_err(Error::Work);
        if let Err(error) = result {
            self.result = None;
            self.failure = Some(error);
        }
        result
    }
    fn advance(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        for _ in 0..32 {
            work.charge(Charge {
                visits: u64::from(self.position < self.source.len()),
                records: 1,
            })
            .map_err(Error::Work)?;
            let Some(byte) = self.source.get(self.position).copied() else {
                let base_end = self.base_end.unwrap_or(self.source.len());
                let form = if base_end == 0 || !self.token_valid {
                    Form::Malformed
                } else {
                    match self.suffix {
                        Suffix::Base => Form::Ordinary,
                        Suffix::Start if self.base_valid => Form::Extended,
                        Suffix::Digits | Suffix::End if self.base_valid => Form::Section {
                            index: self.index,
                            encoded: matches!(self.suffix, Suffix::End),
                        },
                        _ => Form::Malformed,
                    }
                };
                let name = Name {
                    base: Extent {
                        start: 0,
                        end: base_end,
                    },
                    form,
                };
                self.result = Some(name);
                return Ok(Status::Complete(name));
            };
            self.token_valid &= mime_token_octet(byte);
            match self.suffix {
                Suffix::Base if byte == b'*' => {
                    self.base_end = Some(self.position);
                    self.suffix = Suffix::Start;
                }
                Suffix::Base => self.base_valid &= !matches!(byte, b'\'' | b'%'),
                Suffix::Start | Suffix::Digits if byte.is_ascii_digit() => {
                    if matches!(self.suffix, Suffix::Digits) && self.index == 0 {
                        self.suffix = Suffix::Bad;
                    } else if let Some(index) = self
                        .index
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(u64::from(byte - b'0')))
                    {
                        self.index = index;
                        self.suffix = Suffix::Digits;
                    } else {
                        self.suffix = Suffix::Bad;
                    }
                }
                Suffix::Digits if byte == b'*' => {
                    self.suffix = Suffix::End;
                }
                _ => self.suffix = Suffix::Bad,
            }
            self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
        }
        Ok(Status::Yield)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Counter {
        visits: u64,
        records: u64,
        fail_at: Option<u64>,
    }
    impl Work for Counter {
        type Error = u8;
        fn charge(&mut self, charge: Charge) -> Result<(), u8> {
            if self.fail_at.is_some_and(|at| self.records >= at) {
                return Err(9);
            }
            self.visits += charge.visits;
            self.records += charge.records;
            Ok(())
        }
    }
    fn classify(bytes: &[u8]) -> Name {
        let mut cursor = Cursor::new(bytes);
        let mut work = Counter::default();
        loop {
            let before = (work.visits, work.records);
            let result = cursor.poll(&mut work).unwrap();
            assert!(work.visits - before.0 <= 32);
            assert!(work.records - before.1 <= 32);
            if let Status::Complete(name) = result {
                assert_eq!(work.visits, bytes.len() as u64);
                assert_eq!(work.records, bytes.len() as u64 + 1);
                let before = (work.visits, work.records);
                work.fail_at = Some(0);
                assert_eq!(cursor.poll(&mut work), Ok(Status::Complete(name)));
                assert_eq!((work.visits, work.records), before);
                return name;
            }
        }
    }
    #[test]
    fn ordinary_extended_sections_and_bad_spelling() {
        for (name, base, form) in [
            ("filename", "filename", Form::Ordinary),
            ("filename*", "filename", Form::Extended),
            (
                "filename*0",
                "filename",
                Form::Section {
                    index: 0,
                    encoded: false,
                },
            ),
            (
                "filename*0*",
                "filename",
                Form::Section {
                    index: 0,
                    encoded: true,
                },
            ),
            (
                "filename*13*",
                "filename",
                Form::Section {
                    index: 13,
                    encoded: true,
                },
            ),
            (
                "f*18446744073709551615",
                "f",
                Form::Section {
                    index: u64::MAX,
                    encoded: false,
                },
            ),
            ("f*18446744073709551616", "f", Form::Malformed),
            ("filename*00", "filename", Form::Malformed),
            ("filename*01*", "filename", Form::Malformed),
            ("filename**", "filename", Form::Malformed),
            ("filename*1**", "filename", Form::Malformed),
            ("filename*1*x", "filename", Form::Malformed),
            ("filename*-1", "filename", Form::Malformed),
            ("filename*no", "filename", Form::Malformed),
            ("*", "", Form::Malformed),
            ("0", "0", Form::Ordinary),
            ("0*", "0", Form::Extended),
            (
                "0*0",
                "0",
                Form::Section {
                    index: 0,
                    encoded: false,
                },
            ),
            (
                "0*0*",
                "0",
                Form::Section {
                    index: 0,
                    encoded: true,
                },
            ),
            ("*1", "", Form::Malformed),
            ("", "", Form::Malformed),
            ("f%o", "f%o", Form::Ordinary),
            ("f%o*", "f%o", Form::Malformed),
            ("f'o", "f'o", Form::Ordinary),
            ("f'o*1", "f'o", Form::Malformed),
            ("f o", "f o", Form::Malformed),
            ("🐈*1", "🐈", Form::Malformed),
        ] {
            let result = classify(name.as_bytes());
            assert_eq!(&name[result.base.start..result.base.end], base, "{name}");
            assert_eq!(result.form, form, "{name}");
        }
    }
    #[test]
    fn long_names_have_fixed_turns_and_exact_costs() {
        for suffix in ["", "*", "*10*"] {
            let name = format!("{}{}", "a".repeat(65_536), suffix);
            assert_eq!(classify(name.as_bytes()).base.end, 65_536);
        }
    }
    #[test]
    fn every_work_cut_including_eof_is_sticky() {
        let name = b"filename*123*";
        for limit in 0..=name.len() as u64 {
            let mut cursor = Cursor::new(name);
            let mut work = Counter {
                fail_at: Some(limit),
                ..Counter::default()
            };
            loop {
                match cursor.poll(&mut work) {
                    Ok(Status::Yield) => {}
                    Ok(Status::Complete(_)) => panic!("cut admitted"),
                    Err(error) => {
                        assert_eq!(error, Error::Work(9));
                        let mut fresh = Counter::default();
                        assert_eq!(cursor.poll(&mut fresh), Err(error));
                        assert_eq!((fresh.visits, fresh.records), (0, 0));
                        break;
                    }
                }
            }
        }
    }
    #[test]
    fn every_octet_pins_ordinary_and_extended_base_syntax() {
        for byte in 0..=u8::MAX {
            let ordinary = [b'a', byte];
            let expected = if byte == b'*' {
                Form::Extended
            } else if byte.is_ascii_alphanumeric() || b"!#$%&'+-.^_`{|}~".contains(&byte) {
                Form::Ordinary
            } else {
                Form::Malformed
            };
            assert_eq!(classify(&ordinary).form, expected, "ordinary {byte}");
            let extended = [b'a', byte, b'*'];
            let expected = if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`{|}~".contains(&byte) {
                Form::Extended
            } else {
                Form::Malformed
            };
            assert_eq!(classify(&extended).form, expected, "extended {byte}");
        }
    }
    #[test]
    fn explicit_live_admission_retires_cached_classification() {
        let mut cursor = Cursor::new(b"filename*");
        let mut work = Counter::default();
        loop {
            if matches!(cursor.poll(&mut work).unwrap(), Status::Complete(_)) {
                break;
            }
        }
        let before = (work.visits, work.records);
        cursor.check_work(&mut work).unwrap();
        assert_eq!((work.visits, work.records), before);
        work.fail_at = Some(0);
        assert_eq!(cursor.check_work(&mut work), Err(Error::Work(9)));
        assert_eq!(cursor.result, None);
        let mut fresh = Counter::default();
        assert_eq!(cursor.check_work(&mut fresh), Err(Error::Work(9)));
        assert_eq!(cursor.poll(&mut fresh), Err(Error::Work(9)));
        assert_eq!((fresh.visits, fresh.records), (0, 0));
    }
}
