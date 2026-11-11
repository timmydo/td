#![forbid(unsafe_code)]
//! Fallible growth of std collections, with allocation-free errors.
//! Callers retain their own size/work limits. These helpers do not make
//! allocations inside user callbacks, element destructors or Display fallible.

use std::cell::Cell;
use std::fmt;

/// Capacity overflow, allocator refusal, or scoped reservation refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("buffer allocation refused")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy)]
struct Admission {
    left: usize,
    used: usize,
}
thread_local! {
    static ADMISSION: Cell<Option<Admission>> = const { Cell::new(None) };
}

/// Run with at most `limit` new reservation attempts on this thread.
/// Existing capacity costs nothing. Nested scopes cannot replenish an outer
/// allowance; unwinding restores it with successful admissions charged.
/// This is an operation budget and failure-injection seam, not a heap limit:
/// it covers only this module's helpers, not other threads or std allocations.
pub fn with_reservation_limit<T>(limit: usize, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Admission>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ADMISSION.with(|cell| {
                let used = cell.get().map_or(0, |a| a.used);
                cell.set(self.0.map(|a| Admission {
                    left: a.left.saturating_sub(used),
                    used: a.used.saturating_add(used),
                }));
            });
        }
    }
    let previous = ADMISSION.with(|cell| {
        let previous = cell.get();
        cell.set(Some(Admission {
            left: previous.map_or(limit, |a| a.left.min(limit)),
            used: 0,
        }));
        previous
    });
    let _restore = Restore(previous);
    run()
}

fn admit() -> Result<(), Error> {
    ADMISSION.with(|cell| {
        if let Some(mut a) = cell.get() {
            a.left = a.left.checked_sub(1).ok_or(Error)?;
            a.used = a.used.saturating_add(1);
            cell.set(Some(a));
        }
        Ok(())
    })
}

/// Reserve using std's amortized growth strategy. Existing contents survive
/// refusal. `additional` is relative to length, as in std.
pub fn reserve<T>(out: &mut Vec<T>, additional: usize) -> Result<(), Error> {
    if additional > out.capacity().saturating_sub(out.len()) {
        admit()?;
        out.try_reserve(additional).map_err(|_| Error)?;
    }
    Ok(())
}

/// Reserve without requesting speculative capacity. Allocators may still
/// provide more space; no promise is made about physical memory commitment.
pub fn reserve_exact<T>(out: &mut Vec<T>, additional: usize) -> Result<(), Error> {
    if additional > out.capacity().saturating_sub(out.len()) {
        admit()?;
        out.try_reserve_exact(additional).map_err(|_| Error)?;
    }
    Ok(())
}

pub fn with_capacity<T>(capacity: usize) -> Result<Vec<T>, Error> {
    let mut out = Vec::new();
    reserve_exact(&mut out, capacity)?;
    Ok(out)
}

/// Copies values directly; never invokes a user-defined Clone implementation.
pub fn filled<T: Copy>(len: usize, value: T) -> Result<Vec<T>, Error> {
    let mut out = with_capacity(len)?;
    out.resize_with(len, || value);
    Ok(out)
}

pub fn copy<T: Copy>(source: &[T]) -> Result<Vec<T>, Error> {
    let mut out = with_capacity(source.len())?;
    for &value in source {
        out.push(value);
    }
    Ok(out)
}

/// The supplied element is dropped on refusal; its destructor is the
/// caller's responsibility, just as constructing the element is.
pub fn push<T>(out: &mut Vec<T>, value: T) -> Result<(), Error> {
    reserve(out, 1)?;
    out.push(value);
    Ok(())
}

pub fn extend_from_slice<T: Copy>(out: &mut Vec<T>, source: &[T]) -> Result<(), Error> {
    reserve(out, source.len())?;
    for &value in source {
        out.push(value);
    }
    Ok(())
}

pub fn resize<T: Copy>(out: &mut Vec<T>, len: usize, value: T) -> Result<(), Error> {
    reserve_exact(out, len.saturating_sub(out.len()))?;
    out.resize_with(len, || value);
    Ok(())
}

pub fn reserve_map<K, V, S>(
    out: &mut std::collections::HashMap<K, V, S>,
    additional: usize,
) -> Result<(), Error>
where
    K: Eq + std::hash::Hash,
    S: std::hash::BuildHasher,
{
    if additional > out.capacity().saturating_sub(out.len()) {
        admit()?;
        out.try_reserve(additional).map_err(|_| Error)?;
    }
    Ok(())
}

/// Reserves for an insertion even when it might replace an existing key.
/// Hashing/equality and key/value destruction remain caller-owned code.
pub fn insert<K, V, S>(
    out: &mut std::collections::HashMap<K, V, S>,
    key: K,
    value: V,
) -> Result<Option<V>, Error>
where
    K: Eq + std::hash::Hash,
    S: std::hash::BuildHasher,
{
    reserve_map(out, 1)?;
    Ok(out.insert(key, value))
}

pub fn push_back<T>(out: &mut std::collections::VecDeque<T>, value: T) -> Result<(), Error> {
    if out.len() == out.capacity() {
        admit()?;
        out.try_reserve(1).map_err(|_| Error)?;
    }
    out.push_back(value);
    Ok(())
}

pub fn reserve_string(out: &mut String, additional: usize) -> Result<(), Error> {
    if additional > out.capacity().saturating_sub(out.len()) {
        admit()?;
        out.try_reserve(additional).map_err(|_| Error)?;
    }
    Ok(())
}

pub fn push_str(out: &mut String, value: &str) -> Result<(), Error> {
    reserve_string(out, value.len())?;
    out.push_str(value);
    Ok(())
}

pub fn string(value: &str) -> Result<String, Error> {
    let mut out = String::new();
    push_str(&mut out, value)?;
    Ok(out)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatError {
    Allocation,
    Display,
}
impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Allocation => "text allocation refused",
            Self::Display => "value formatting failed",
        })
    }
}
impl std::error::Error for FormatError {}

/// Formats into fallibly grown storage. Display implementations must themselves
/// be allocation-free (as primitive values and td's small error enums are).
pub fn format(args: fmt::Arguments<'_>) -> Result<String, FormatError> {
    struct Writer {
        text: String,
        allocation_failed: bool,
    }
    impl fmt::Write for Writer {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            if self.allocation_failed || push_str(&mut self.text, text).is_err() {
                self.allocation_failed = true;
                return Err(fmt::Error);
            }
            Ok(())
        }
    }
    let mut writer = Writer {
        text: String::new(),
        allocation_failed: false,
    };
    let formatted = fmt::write(&mut writer, args);
    if writer.allocation_failed {
        Err(FormatError::Allocation)
    } else if formatted.is_err() {
        Err(FormatError::Display)
    } else {
        Ok(writer.text)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn capacity_and_admission_failures_preserve_contents() {
        let mut bytes = copy(b"abc").unwrap();
        assert_eq!(reserve_exact(&mut bytes, usize::MAX), Err(Error));
        assert_eq!(bytes, b"abc");
        with_reservation_limit(0, || {
            let additional = bytes.capacity() + 1;
            assert_eq!(reserve(&mut bytes, additional), Err(Error));
            assert_eq!(filled(1, 0u8), Err(Error));
            assert_eq!(string("hello"), Err(Error));
            assert_eq!(format(format_args!("{}", 42)), Err(FormatError::Allocation));
        });
        assert_eq!(bytes, b"abc");
        push(&mut bytes, b'd').unwrap();
        assert_eq!(bytes, b"abcd");
    }

    #[test]
    fn spare_capacity_and_zero_sized_types_do_not_allocate() {
        let mut bytes = with_capacity(8).unwrap();
        with_reservation_limit(0, || {
            push(&mut bytes, 1u8).unwrap();
            extend_from_slice(&mut bytes, &[2, 3]).unwrap();
            resize(&mut bytes, 8, 4).unwrap();
            resize(&mut bytes, 1, 0).unwrap();
            let mut zst = Vec::new();
            push(&mut zst, ()).unwrap();
            assert_eq!(zst.len(), 1);
        });
        assert_eq!(bytes, [1]);
    }

    #[test]
    fn nested_scopes_charge_the_outer_scope_and_unwinding_restores_it() {
        with_reservation_limit(2, || {
            with_reservation_limit(100, || {
                filled(1, 1u8).unwrap();
            });
            let failed = std::panic::catch_unwind(|| {
                with_reservation_limit(1, || {
                    filled(1, 1u8).unwrap();
                    panic!("exercise scope cleanup");
                })
            });
            assert!(failed.is_err());
            assert_eq!(filled(1, 1u8), Err(Error));
        });
        assert_eq!(filled(1, 1u8).unwrap(), [1]);
    }

    #[test]
    fn formatting_reports_the_origin_of_failure() {
        struct Bad;
        impl fmt::Display for Bad {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert_eq!(format(format_args!("{Bad}")), Err(FormatError::Display));
        assert_eq!(format(format_args!("{} {}", "λ", 42)).unwrap(), "λ 42");
    }

    #[test]
    #[allow(clippy::non_canonical_clone_impl)]
    fn copy_helpers_never_invoke_user_clone() {
        #[derive(Copy)]
        struct Value;
        impl Clone for Value {
            fn clone(&self) -> Self {
                panic!("Clone must not run")
            }
        }
        let mut values = filled(2, Value).unwrap();
        resize(&mut values, 3, Value).unwrap();
        extend_from_slice(&mut values, &[Value]).unwrap();
        assert_eq!(copy(&values).unwrap().len(), 4);
    }

    #[test]
    fn maps_queues_and_thread_scopes_refuse_independently() {
        let mut map = std::collections::HashMap::new();
        let mut queue = std::collections::VecDeque::new();
        with_reservation_limit(0, || {
            assert_eq!(insert(&mut map, 1, 2), Err(Error));
            assert_eq!(push_back(&mut queue, 3), Err(Error));
            assert!(std::thread::spawn(|| filled(1, 4u8))
                .join()
                .unwrap()
                .is_ok());
            assert_eq!(filled(1, 4u8), Err(Error));
        });
        assert!(map.is_empty());
        assert!(queue.is_empty());
        assert_eq!(insert(&mut map, 1, 2).unwrap(), None);
        push_back(&mut queue, 3).unwrap();
        assert_eq!(queue.pop_front(), Some(3));
    }
}
