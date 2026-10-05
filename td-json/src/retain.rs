//! Fixed caller-reserved fragment storage; validity and admission stay external.
#![cfg_attr(clippy, deny(warnings))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Capacity => "retained fragment capacity",
            Self::InvalidState => "invalid retained fragment state",
        })
    }
}
impl std::error::Error for Error {}
/// Owns one fixed backing identity and checked prefix length without allocation.
/// The producer reports only bytes it actually wrote through tail. A refusal is
/// sticky; replacing a source or retrying with another backing renews no authority.
/// All bytes are provisional: the enclosing owner proves producer completion and
/// fresh final admission before publication or consuming into_slice.
/// This supplies no JSON validation, source, work, presence or transaction grant.
pub struct Window<'w> {
    backing: &'w mut [u8],
    used: usize,
    failure: Option<Error>,
}
impl<'w> Window<'w> {
    pub const fn new(backing: &'w mut [u8]) -> Self {
        Self {
            backing,
            used: 0,
            failure: None,
        }
    }
    pub fn tail(&mut self) -> Result<&mut [u8], Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.used == self.backing.len() {
            self.failure = Some(Error::Capacity);
            return Err(Error::Capacity);
        }
        self.backing.get_mut(self.used..).ok_or_else(|| {
            self.failure = Some(Error::InvalidState);
            Error::InvalidState
        })
    }
    /// Record the producer's reported prefix after one successful writing turn.
    pub fn advance(&mut self, written: usize) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        match self.used.checked_add(written) {
            Some(used) if used <= self.backing.len() => {
                self.used = used;
                Ok(())
            }
            _ => {
                self.failure = Some(Error::InvalidState);
                Err(Error::InvalidState)
            }
        }
    }
    /// Provisional bytes, never producer-completion or publication evidence.
    pub fn provisional(&self) -> Option<&[u8]> {
        if self.failure.is_some() {
            None
        } else {
            self.backing.get(..self.used)
        }
    }
    /// The enclosing owner must first prove completion and fresh admission.
    pub fn into_slice(self) -> Result<&'w [u8], Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.backing.get(..self.used).ok_or(Error::InvalidState)
    }
}
const _: () = assert!(std::mem::size_of::<Window<'_>>() <= 4 * std::mem::size_of::<usize>());
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_prefix_retains_one_backing_and_exact_written_bytes() {
        let mut bytes = [0xa5; 8];
        let identity = bytes.as_ptr();
        let mut window = Window::new(&mut bytes);
        assert_eq!(window.provisional(), Some([].as_slice()));
        window.advance(0).unwrap();
        window
            .tail()
            .unwrap()
            .get_mut(..2)
            .unwrap()
            .copy_from_slice(b"ab");
        window.advance(2).unwrap();
        assert_eq!(window.provisional(), Some(b"ab".as_slice()));
        window
            .tail()
            .unwrap()
            .get_mut(..3)
            .unwrap()
            .copy_from_slice(b"cde");
        window.advance(3).unwrap();
        let value = window.into_slice().unwrap();
        assert_eq!(value.as_ptr(), identity);
        assert_eq!(value, b"abcde");
        assert_eq!(bytes.get(5..), Some([0xa5; 3].as_slice()));
    }
    #[test]
    fn empty_full_oversized_and_overflow_refusals_are_sticky() {
        for size in 0..=4 {
            let mut backing = [0; 4];
            let mut window = Window::new(backing.get_mut(..size).unwrap());
            window.advance(size).unwrap();
            assert_eq!(window.tail().err(), Some(Error::Capacity));
            assert_eq!(window.advance(0), Err(Error::Capacity));
            assert_eq!(window.provisional(), None);
            assert_eq!(window.into_slice().err(), Some(Error::Capacity));
        }
        for oversized in [5, usize::MAX] {
            let mut backing = [0; 4];
            let mut window = Window::new(&mut backing);
            window.advance(1).unwrap();
            assert_eq!(window.advance(oversized), Err(Error::InvalidState));
            assert_eq!(window.tail().err(), Some(Error::InvalidState));
            assert_eq!(window.advance(0), Err(Error::InvalidState));
            assert_eq!(window.provisional(), None);
            assert_eq!(window.into_slice().err(), Some(Error::InvalidState));
        }
        let mut backing = [0; 4];
        let mut window = Window::new(&mut backing);
        window.tail().unwrap().fill(7);
        window.advance(4).unwrap();
        assert_eq!(window.into_slice().unwrap(), [7; 4]);
        let mut backing = [];
        assert_eq!(Window::new(&mut backing).into_slice().unwrap(), []);
    }
}
