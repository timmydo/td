//! Fallible codec buffers; failure injection is confined to unit tests.
use std::collections::TryReserveError;

pub(crate) fn reserve<T>(buffer: &mut Vec<T>, additional: usize) -> Result<(), TryReserveError> {
    #[cfg(test)]
    if additional > buffer.capacity().saturating_sub(buffer.len()) {
        let fail = FAILURE.with(|left| match left.get() {
            Some(0) => true,
            Some(n) => {
                left.set(Some(n - 1));
                false
            }
            None => false,
        });
        if fail {
            return Vec::<u8>::new().try_reserve_exact(usize::MAX);
        }
    }
    buffer.try_reserve_exact(additional)
}

pub(crate) fn filled<T: Copy>(len: usize, value: T) -> Result<Vec<T>, TryReserveError> {
    let mut out = Vec::new();
    reserve(&mut out, len)?;
    out.resize(len, value);
    Ok(out)
}

pub(crate) fn copy<T: Copy>(source: &[T]) -> Result<Vec<T>, TryReserveError> {
    let mut out = Vec::new();
    reserve(&mut out, source.len())?;
    out.extend_from_slice(source);
    Ok(out)
}

#[cfg(test)]
thread_local! { static FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
pub(crate) fn failing<T>(after: usize, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FAILURE.with(|left| left.set(None));
        }
    }
    FAILURE.with(|left| left.set(Some(after)));
    let _reset = Reset;
    run()
}
