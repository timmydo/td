//! Bounded non-routing configuration bytes; no schema, file trust or publication.
use super::values;
use std::{
    fmt,
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
};
pub const MAX_BYTES: usize = 192 * 1024;
// Uniqueness only; no data synchronization and no reset after a builder drops.
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
pub(crate) static TEST_CONSTRUCTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    Reference,
    Utf8,
    DnsName,
    OwnerExhausted,
    Contended,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_text_capacity",
            Self::Reference => "config_text_reference",
            Self::Utf8 => "config_text_utf8",
            Self::DnsName => "config_text_dns_name",
            Self::OwnerExhausted => "config_text_owner_exhausted",
            Self::Contended => "config_text_contended",
        }
    }
}
impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
impl std::error::Error for Code {}
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct Span {
    offset: u32,
    length: u32,
}
impl Span {
    pub(super) const EMPTY: Self = Self {
        offset: 0,
        length: 0,
    };
    fn read(self, bytes: &[u8]) -> Result<&[u8], Code> {
        let start = usize::try_from(self.offset).map_err(|_| Code::Reference)?;
        let count = usize::try_from(self.length).map_err(|_| Code::Reference)?;
        bytes
            .get(start..start.checked_add(count).ok_or(Code::Reference)?)
            .ok_or(Code::Reference)
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
/// Process-local reference, usable only with its issuing builder or frozen view.
/// Not a persistent descriptor or a serialized offset.
pub struct Handle {
    owner: NonZeroU64,
    span: Span,
}
impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Handle(<redacted>)")
    }
}
fn claim(counter: &AtomicU64, observed: u64) -> Result<NonZeroU64, Code> {
    let owner = NonZeroU64::new(observed).ok_or(Code::OwnerExhausted)?;
    let next = observed.checked_add(1).ok_or(Code::OwnerExhausted)?;
    counter
        .compare_exchange(observed, next, Ordering::Relaxed, Ordering::Relaxed)
        .map_err(|_| Code::Contended)?;
    Ok(owner)
}
pub struct Builder<'a> {
    storage: &'a mut [u8],
    used: usize,
    owner: NonZeroU64,
}
impl fmt::Debug for Builder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TextBuilder(<redacted>)")
    }
}
impl<'a> Builder<'a> {
    pub(super) fn owner(&self) -> NonZeroU64 {
        self.owner
    }
    // Strip the ticket only after checking this builder owns the handle.
    pub(super) fn compact(&self, handle: Handle) -> Result<Span, Code> {
        self.read(handle)?;
        Ok(handle.span)
    }
    /// One bounded ownership-ticket attempt; contention leaves storage untouched.
    /// The control worker may retry in a later bounded operation.
    pub fn new(storage: &'a mut [u8]) -> Result<Self, Code> {
        if storage.len() > MAX_BYTES {
            return Err(Code::Capacity);
        }
        let owner = claim(&NEXT_OWNER, NEXT_OWNER.load(Ordering::Relaxed))?;
        Ok(Self {
            storage,
            used: 0,
            owner,
        })
    }
    pub fn used(&self) -> usize {
        self.used
    }
    pub fn capacity(&self) -> usize {
        self.storage.len()
    }
    /// Capacity failure preserves both used length and the backing bytes.
    /// Bytes need not be UTF-8; text() validates the selected reference on demand.
    pub fn append(&mut self, input: &[u8]) -> Result<Handle, Code> {
        self.copy(input, false)
    }
    pub fn append_dns(&mut self, input: &str) -> Result<Handle, Code> {
        values::dns_name(input).map_err(|_| Code::DnsName)?;
        self.copy(input.as_bytes(), true)
    }
    pub fn append_certificate_name(&mut self, input: &str) -> Result<Handle, Code> {
        values::certificate_name(input).map_err(|_| Code::DnsName)?;
        self.copy(input.as_bytes(), true)
    }
    fn copy(&mut self, input: &[u8], fold: bool) -> Result<Handle, Code> {
        let end = self.used.checked_add(input.len()).ok_or(Code::Capacity)?;
        let span = Span {
            offset: u32::try_from(self.used).map_err(|_| Code::Capacity)?,
            length: u32::try_from(input.len()).map_err(|_| Code::Capacity)?,
        };
        let destination = self.storage.get_mut(self.used..end).ok_or(Code::Capacity)?;
        destination.copy_from_slice(input);
        if fold {
            destination.make_ascii_lowercase();
        }
        self.used = end;
        Ok(Handle {
            owner: self.owner,
            span,
        })
    }
    pub fn read(&self, handle: Handle) -> Result<&[u8], Code> {
        checked_read(
            self.owner,
            self.storage.get(..self.used).ok_or(Code::Reference)?,
            handle,
        )
    }
    pub fn text(&self, handle: Handle) -> Result<&str, Code> {
        std::str::from_utf8(self.read(handle)?).map_err(|_| Code::Utf8)
    }
    pub(super) fn borrowed_view(&self) -> Result<View<'_>, Code> {
        Ok(View {
            bytes: self.storage.get(..self.used).ok_or(Code::Reference)?,
            owner: self.owner,
        })
    }
    /// Freeze only the initialized prefix; no reset, scrubbing or publication.
    pub fn freeze(self) -> Result<View<'a>, Code> {
        Ok(View {
            bytes: self.storage.get(..self.used).ok_or(Code::Reference)?,
            owner: self.owner,
        })
    }
}
#[derive(Clone, Copy)]
pub struct View<'a> {
    bytes: &'a [u8],
    owner: NonZeroU64,
}
impl fmt::Debug for View<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TextView(<redacted>)")
    }
}
impl<'a> View<'a> {
    pub(super) fn owner(self) -> NonZeroU64 {
        self.owner
    }
    pub(super) fn read_span(self, owner: NonZeroU64, span: Span) -> Result<&'a [u8], Code> {
        if self.owner != owner {
            return Err(Code::Reference);
        }
        span.read(self.bytes)
    }
    pub fn used(self) -> usize {
        self.bytes.len()
    }
    pub fn read(self, handle: Handle) -> Result<&'a [u8], Code> {
        checked_read(self.owner, self.bytes, handle)
    }
    pub fn text(self, handle: Handle) -> Result<&'a str, Code> {
        std::str::from_utf8(self.read(handle)?).map_err(|_| Code::Utf8)
    }
}
fn checked_read(owner: NonZeroU64, bytes: &[u8], handle: Handle) -> Result<&[u8], Code> {
    if owner != handle.owner {
        return Err(Code::Reference);
    }
    handle.span.read(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn capacity_failures_preserve_all_bytes_and_empty_values_fit() {
        let _serial = TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = [0x55; 5];
        let mut builder = Builder::new(&mut bytes).unwrap();
        let first = builder.append(b"AbC").unwrap();
        assert_eq!(builder.used(), 3);
        assert_eq!(builder.capacity(), 5);
        assert_eq!(builder.append(b"def"), Err(Code::Capacity));
        assert_eq!(builder.used(), 3);
        assert_eq!(builder.storage, &[b'A', b'b', b'C', 0x55, 0x55]);
        let last = builder.append(b"de").unwrap();
        let empty = builder.append(b"").unwrap();
        let view = builder.freeze().unwrap();
        assert_eq!(view.used(), 5);
        assert_eq!(view.text(first), Ok("AbC"));
        assert_eq!(view.read(last), Ok(&b"de"[..]));
        assert_eq!(view.read(empty), Ok(&b""[..]));
        let mut zero = [];
        let mut empty_builder = Builder::new(&mut zero).unwrap();
        assert!(empty_builder.append(b"").is_ok());
        assert_eq!(empty_builder.append(b"x"), Err(Code::Capacity));
    }
    #[test]
    fn handles_reject_other_owners_including_reused_storage_and_empty_spans() {
        let _serial = TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = [0; 8];
        let (old, old_empty) = {
            let mut builder = Builder::new(&mut bytes).unwrap();
            (
                builder.append(b"old").unwrap(),
                builder.append(b"").unwrap(),
            )
        };
        let mut builder = Builder::new(&mut bytes).unwrap();
        let new = builder.append(b"new").unwrap();
        assert_eq!(builder.read(old), Err(Code::Reference));
        assert_eq!(builder.read(old_empty), Err(Code::Reference));
        assert_eq!(builder.text(new), Ok("new"));
        let view = builder.freeze().unwrap();
        assert_eq!(view.read(old), Err(Code::Reference));
        assert_eq!(view.read(old_empty), Err(Code::Reference));
        let mut separate = [0; 8];
        let mut other = Builder::new(&mut separate).unwrap();
        let foreign = other.append(b"new").unwrap();
        assert_eq!(view.read(foreign), Err(Code::Reference));
        assert_ne!(new, foreign);
    }
    #[test]
    fn reference_ranges_use_written_prefix_and_text_checks_utf8() {
        let _serial = TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = [0x55; 16];
        let mut builder = Builder::new(&mut bytes).unwrap();
        let binary = builder.append(&[0xff, 0]).unwrap();
        let text = builder.append("é".as_bytes()).unwrap();
        assert_eq!(builder.text(binary), Err(Code::Utf8));
        assert_eq!(builder.read(binary), Ok(&[0xff, 0][..]));
        assert_eq!(builder.text(text), Ok("é"));
        for span in [
            Span {
                offset: 4,
                length: 1,
            },
            Span {
                offset: 5,
                length: 0,
            },
            Span {
                offset: u32::MAX,
                length: u32::MAX,
            },
        ] {
            let bad = Handle {
                owner: builder.owner,
                span,
            };
            assert_eq!(builder.read(bad), Err(Code::Reference));
        }
        let split = Handle {
            owner: builder.owner,
            span: Span {
                offset: 2,
                length: 1,
            },
        };
        assert_eq!(builder.text(split), Err(Code::Utf8));
        let end = Handle {
            owner: builder.owner,
            span: Span {
                offset: 4,
                length: 0,
            },
        };
        let view = builder.freeze().unwrap();
        assert_eq!(view.used(), 4);
        let past_end = Handle {
            owner: view.owner,
            span: Span {
                offset: 4,
                length: 1,
            },
        };
        assert_eq!(view.read(past_end), Err(Code::Reference));
        assert_eq!(view.text(split), Err(Code::Utf8));
        assert_eq!(view.read(end), Ok(&b""[..]));
        assert_eq!(view.text(text), Ok("é"));
    }
    #[test]
    fn canonical_dns_copy_has_separate_normal_and_certificate_bounds() {
        let _serial = TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = [0x55; 1024];
        let mut builder = Builder::new(&mut bytes).unwrap();
        let mixed = builder.append_dns("MX.Example.TEST").unwrap();
        assert_eq!(builder.text(mixed), Ok("mx.example.test"));
        let max = format!(
            "{}.{}.{}.{}",
            "A".repeat(63),
            "B".repeat(63),
            "C".repeat(63),
            "D".repeat(51)
        );
        assert_eq!(max.len(), values::MAX_DOMAIN_BYTES);
        let host = builder.append_dns(&max).unwrap();
        assert_eq!(builder.text(host).unwrap(), max.to_ascii_lowercase());
        let before = builder.used();
        let prior_bytes = builder.storage.to_vec();
        assert_eq!(builder.append_dns(&(max.clone() + "E")), Err(Code::DnsName));
        assert_eq!(builder.append_dns("bad..test"), Err(Code::DnsName));
        assert_eq!(builder.used(), before);
        assert_eq!(builder.storage, prior_bytes);
        let certificate = max + &"E".repeat(10);
        assert_eq!(certificate.len(), values::MAX_CERTIFICATE_NAME_BYTES);
        let cert = builder.append_certificate_name(&certificate).unwrap();
        assert_eq!(
            builder.text(cert).unwrap(),
            certificate.to_ascii_lowercase()
        );
        let before = builder.used();
        let prior_bytes = builder.storage.to_vec();
        assert_eq!(
            builder.append_certificate_name(&(certificate + "F")),
            Err(Code::DnsName)
        );
        assert_eq!(builder.used(), before);
        assert_eq!(builder.storage, prior_bytes);
    }
    #[test]
    fn full_partition_and_descriptor_sizes_are_bounded() {
        let _serial = TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            MAX_BYTES + crate::config::routing::MAX_TEXT_BYTES,
            512 * 1024
        );
        assert_eq!(std::mem::size_of::<Span>(), 8);
        assert_eq!(std::mem::size_of::<Handle>(), 16);
        assert!(std::mem::size_of::<Builder<'_>>() <= 32);
        assert!(std::mem::size_of::<View<'_>>() <= 24);
        let mut storage = vec![0; MAX_BYTES];
        let mut builder = Builder::new(&mut storage).unwrap();
        let value = vec![b'x'; MAX_BYTES];
        let full = builder.append(&value).unwrap();
        assert_eq!(builder.read(full).unwrap().len(), MAX_BYTES);
        assert_eq!(builder.append(b"x"), Err(Code::Capacity));
        let mut oversized = vec![0x55; MAX_BYTES + 1];
        assert_eq!(Builder::new(&mut oversized).err(), Some(Code::Capacity));
        assert!(oversized.iter().all(|b| *b == 0x55));
    }
    #[test]
    fn owner_issuance_is_bounded_and_never_wraps_or_reuses() {
        let counter = AtomicU64::new(1);
        assert_eq!(claim(&counter, 1).unwrap().get(), 1);
        assert_eq!(claim(&counter, 1), Err(Code::Contended));
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        assert_eq!(claim(&counter, 2).unwrap().get(), 2);
        assert_eq!(claim(&counter, 0), Err(Code::OwnerExhausted));
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(claim(&counter, u64::MAX - 1).unwrap().get(), u64::MAX - 1);
        assert_eq!(claim(&counter, u64::MAX), Err(Code::OwnerExhausted));
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
    #[test]
    fn debug_and_error_chains_do_not_disclose_content() {
        let _serial = TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = [0; 64];
        let mut builder = Builder::new(&mut bytes).unwrap();
        let handle = builder.append(b"secret-fixture").unwrap();
        assert_eq!(format!("{builder:?}"), "TextBuilder(<redacted>)");
        assert_eq!(format!("{handle:?}"), "Handle(<redacted>)");
        let view = builder.freeze().unwrap();
        assert_eq!(format!("{view:?}"), "TextView(<redacted>)");
        for (code, name) in [
            (Code::Capacity, "config_text_capacity"),
            (Code::Reference, "config_text_reference"),
            (Code::Utf8, "config_text_utf8"),
            (Code::DnsName, "config_text_dns_name"),
            (Code::OwnerExhausted, "config_text_owner_exhausted"),
            (Code::Contended, "config_text_contended"),
        ] {
            assert_eq!(code.name(), name);
            assert_eq!(code.to_string(), name);
            assert!(std::error::Error::source(&code).is_none());
        }
    }
    #[test]
    fn compact_conversion_and_span_reads_check_owner_and_written_prefix() {
        let _lock = TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = [0; 8];
        let stale = {
            let mut b = Builder::new(&mut bytes).unwrap();
            b.append(b"old").unwrap()
        };
        let mut b = Builder::new(&mut bytes).unwrap();
        let current = b.append(b"new").unwrap();
        assert_eq!(b.compact(stale).err(), Some(Code::Reference));
        let mut other_bytes = [0; 8];
        let mut other = Builder::new(&mut other_bytes).unwrap();
        let foreign = other.append(b"new").unwrap();
        assert_eq!(b.compact(foreign).err(), Some(Code::Reference));
        let outside = Handle {
            owner: b.owner,
            span: Span {
                offset: 3,
                length: 1,
            },
        };
        assert_eq!(b.compact(outside).err(), Some(Code::Reference));
        let span = b.compact(current).unwrap();
        let owner = b.owner;
        let borrowed = b.borrowed_view().unwrap();
        assert_eq!(borrowed.read_span(owner, span), Ok(&b"new"[..]));
        assert_eq!(borrowed.read_span(other.owner, span), Err(Code::Reference));
        assert_eq!(
            borrowed.read_span(owner, outside.span),
            Err(Code::Reference)
        );
        b.append(b"more").unwrap();
        assert_eq!(b.freeze().unwrap().used(), 7);
    }
}
