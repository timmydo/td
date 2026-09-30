//! Final-link libc forwarding for the separate native observation executable.
use super::allocation_registry::Registry;
use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

pub static TD_MTA_NATIVE_REGISTRY: Registry<65536> = Registry::new();
static CALLS: [AtomicUsize; 6] = [const { AtomicUsize::new(0) }; 6];
static NULLS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // Const, drop-free TLS avoids allocator calls while entering a wrapper.
    static ENTERED: Cell<bool> = const { Cell::new(false) };
}

struct Entry;
impl Drop for Entry {
    fn drop(&mut self) {
        if ENTERED.try_with(|entered| entered.set(false)).is_err() {
            TD_MTA_NATIVE_REGISTRY.invalidate();
        }
    }
}

fn enter() -> Option<Entry> {
    match ENTERED.try_with(|entered| {
        if entered.replace(true) {
            None
        } else {
            Some(Entry)
        }
    }) {
        Ok(entry) => entry,
        Err(_) => {
            TD_MTA_NATIVE_REGISTRY.invalidate();
            None
        }
    }
}

fn increment(counter: &AtomicUsize) {
    let mut old = counter.load(Ordering::Relaxed);
    loop {
        let Some(next) = old.checked_add(1) else {
            TD_MTA_NATIVE_REGISTRY.invalidate();
            return;
        };
        match counter.compare_exchange_weak(old, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(current) => old = current,
        }
    }
}

fn call(index: usize) {
    if let Some(counter) = CALLS.get(index) {
        increment(counter);
    } else {
        TD_MTA_NATIVE_REGISTRY.invalidate();
    }
}

fn allocated(pointer: *mut c_void, size: Option<usize>) {
    if pointer.is_null() {
        increment(&NULLS);
    } else if let Some(size) = size {
        let _ = TD_MTA_NATIVE_REGISTRY.insert(pointer as usize, size);
    } else {
        TD_MTA_NATIVE_REGISTRY.invalidate();
    }
}

pub fn calls() -> [usize; 6] {
    std::array::from_fn(|index| CALLS.get(index).map_or(0, |c| c.load(Ordering::Relaxed)))
}

pub fn instrumentation_storage() -> [usize; 3] {
    [
        std::mem::size_of_val(&TD_MTA_NATIVE_REGISTRY),
        std::mem::size_of_val(&CALLS) + std::mem::size_of_val(&NULLS),
        std::mem::size_of::<Cell<bool>>(),
    ]
}

pub fn null_returns() -> usize {
    NULLS.load(Ordering::Relaxed)
}

#[allow(unsafe_code)]
unsafe extern "C" {
    fn __real_malloc(size: usize) -> *mut c_void;
    fn __real_calloc(count: usize, size: usize) -> *mut c_void;
    fn __real_realloc(pointer: *mut c_void, size: usize) -> *mut c_void;
    fn __real_free(pointer: *mut c_void);
    fn __real_posix_memalign(out: *mut *mut c_void, alignment: usize, size: usize) -> c_int;
    fn __real_aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
}

/// # Safety
/// The caller follows the C malloc contract.
#[allow(unsafe_code)]
#[no_mangle]
pub unsafe extern "C" fn __wrap_malloc(size: usize) -> *mut c_void {
    let Some(_entry) = enter() else {
        // A nested libc call is already owned by the outer boundary operation.
        return unsafe { __real_malloc(size) };
    };
    call(0);
    // The C caller supplies the allocator's unchanged size contract.
    let result = unsafe { __real_malloc(size) };
    allocated(result, Some(size));
    result
}

/// # Safety
/// The caller follows the C calloc contract.
#[allow(unsafe_code)]
#[no_mangle]
pub unsafe extern "C" fn __wrap_calloc(count: usize, size: usize) -> *mut c_void {
    let Some(_entry) = enter() else {
        // A nested libc call is already owned by the outer boundary operation.
        return unsafe { __real_calloc(count, size) };
    };
    call(1);
    // Overflow is for libc to reject; forward the original factors.
    let result = unsafe { __real_calloc(count, size) };
    allocated(result, count.checked_mul(size));
    result
}

/// # Safety
/// The pointer is null or a live compatible allocation; size follows libc.
#[allow(unsafe_code)]
#[no_mangle]
pub unsafe extern "C" fn __wrap_realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    let Some(_entry) = enter() else {
        // A nested libc call is already owned by the outer boundary operation.
        return unsafe { __real_realloc(pointer, size) };
    };
    call(2);
    let old = if pointer.is_null() {
        None
    } else {
        TD_MTA_NATIVE_REGISTRY.remove(pointer as usize).ok()
    };
    if !pointer.is_null() && size == 0 {
        TD_MTA_NATIVE_REGISTRY.invalidate();
    }
    // The caller owns pointer or supplies null; preserve libc failure behavior.
    let result = unsafe { __real_realloc(pointer, size) };
    if result.is_null() {
        increment(&NULLS);
        if size != 0 {
            if let Some(old) = old {
                let _ = TD_MTA_NATIVE_REGISTRY.insert(pointer as usize, old);
            }
        }
    } else {
        allocated(result, Some(size));
    }
    result
}

/// # Safety
/// The pointer is null or a live compatible allocation freed exactly once.
#[allow(unsafe_code)]
#[no_mangle]
pub unsafe extern "C" fn __wrap_free(pointer: *mut c_void) {
    let Some(_entry) = enter() else {
        // A nested libc call is already owned by the outer boundary operation.
        return unsafe { __real_free(pointer) };
    };
    call(3);
    if !pointer.is_null() {
        let _ = TD_MTA_NATIVE_REGISTRY.remove(pointer as usize);
    }
    // Remove before libc can make this address available to another thread.
    unsafe { __real_free(pointer) };
}

/// # Safety
/// The output points to writable pointer storage; libc validates alignment.
#[allow(unsafe_code)]
#[no_mangle]
pub unsafe extern "C" fn __wrap_posix_memalign(
    out: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    let Some(_entry) = enter() else {
        // A nested libc call is already owned by the outer boundary operation.
        return unsafe { __real_posix_memalign(out, alignment, size) };
    };
    call(4);
    // The C caller supplies writable output storage and the alignment contract.
    let result = unsafe { __real_posix_memalign(out, alignment, size) };
    if result == 0 {
        // Read only after success; leave the output completely untouched.
        allocated(unsafe { *out }, Some(size));
    }
    result
}

/// # Safety
/// Alignment and size follow the libc aligned_alloc contract.
#[allow(unsafe_code)]
#[no_mangle]
pub unsafe extern "C" fn __wrap_aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    let Some(_entry) = enter() else {
        // A nested libc call is already owned by the outer boundary operation.
        return unsafe { __real_aligned_alloc(alignment, size) };
    };
    call(5);
    // Forward alignment and size unchanged, including libc's failure result.
    let result = unsafe { __real_aligned_alloc(alignment, size) };
    allocated(result, Some(size));
    result
}
