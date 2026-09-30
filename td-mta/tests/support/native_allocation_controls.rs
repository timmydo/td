//! Positive controls through ordinary libc names, redirected only at final link.
use super::native_allocator_bridge::{calls, null_returns, TD_MTA_NATIVE_REGISTRY as REGISTRY};
use std::ffi::{c_int, c_void};
use std::hint::black_box;

#[allow(unsafe_code)]
unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn calloc(count: usize, size: usize) -> *mut c_void;
    fn realloc(pointer: *mut c_void, size: usize) -> *mut c_void;
    fn free(pointer: *mut c_void);
    fn posix_memalign(out: *mut *mut c_void, alignment: usize, size: usize) -> c_int;
    fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
}

#[allow(unsafe_code)]
pub fn run(zero_resize: bool) {
    // Opaque callees resist allocator-call elimination; exact call deltas below
    // must still prove the pinned compiled controls executed.
    let malloc = black_box(malloc as unsafe extern "C" fn(usize) -> *mut c_void);
    let calloc = black_box(calloc as unsafe extern "C" fn(usize, usize) -> *mut c_void);
    let realloc = black_box(realloc as unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void);
    let free = black_box(free as unsafe extern "C" fn(*mut c_void));
    let posix_memalign =
        black_box(posix_memalign as unsafe extern "C" fn(*mut *mut c_void, usize, usize) -> c_int);
    let aligned_alloc =
        black_box(aligned_alloc as unsafe extern "C" fn(usize, usize) -> *mut c_void);
    let before = REGISTRY.snapshot();
    let count_before = calls();
    let null_before = null_returns();
    assert!(!before.invalid);
    let zero_owned;
    // Every extent below follows a checked successful allocation. Failed realloc
    // keeps the old pointer; successful realloc transfers its ownership.
    unsafe {
        *__errno_location() = 37;
        free(std::ptr::null_mut());
        assert_eq!(*__errno_location(), 37);
        let mut pointer = malloc(black_box(31));
        assert!(!pointer.is_null());
        pointer.cast::<u8>().write_bytes(0x5a, 31);
        let live = REGISTRY.snapshot();
        assert_eq!(live.bytes, before.bytes + 31);
        assert_eq!(live.blocks, before.blocks + 1);
        *__errno_location() = 0;
        assert!(realloc(pointer, black_box(usize::MAX)).is_null());
        assert_eq!(*__errno_location(), 12); // ENOMEM on the qualified Linux target.
        assert_eq!(REGISTRY.snapshot().bytes, live.bytes);
        assert!(std::slice::from_raw_parts(pointer.cast::<u8>(), 31)
            .iter()
            .all(|&b| b == 0x5a));
        let grown = realloc(pointer, black_box(8192));
        assert!(!grown.is_null());
        pointer = grown;
        assert!(std::slice::from_raw_parts(pointer.cast::<u8>(), 31)
            .iter()
            .all(|&b| b == 0x5a));
        assert_eq!(REGISTRY.snapshot().bytes, before.bytes + 8192);
        let shrunk = realloc(pointer, black_box(7));
        assert!(!shrunk.is_null());
        pointer = shrunk;
        assert_eq!(REGISTRY.snapshot().bytes, before.bytes + 7);
        free(pointer);
        let pointer = realloc(std::ptr::null_mut(), black_box(17));
        assert!(!pointer.is_null());
        free(pointer);
        let pointer = calloc(black_box(7), black_box(9));
        assert!(!pointer.is_null());
        assert!(std::slice::from_raw_parts(pointer.cast::<u8>(), 63)
            .iter()
            .all(|&b| b == 0));
        free(pointer);
        assert!(calloc(black_box(usize::MAX), black_box(2)).is_null());
        let mut out = std::ptr::null_mut();
        assert_eq!(
            posix_memalign(&mut out, black_box(4096), black_box(4096)),
            0
        );
        assert!(!out.is_null());
        assert_eq!(out as usize % 4096, 0);
        let retained = out;
        assert_ne!(posix_memalign(&mut out, black_box(3), black_box(4096)), 0);
        assert_eq!(out, retained);
        assert_eq!(
            posix_memalign(&mut out, black_box(4096), black_box(usize::MAX)),
            12
        );
        assert_eq!(out, retained);
        free(out);
        let pointer = aligned_alloc(black_box(4096), black_box(8192));
        assert!(!pointer.is_null());
        assert_eq!(pointer as usize % 4096, 0);
        free(pointer);
        let pointer = malloc(black_box(0));
        zero_owned = !pointer.is_null();
        if zero_owned {
            assert_eq!(REGISTRY.snapshot().blocks, before.blocks + 1);
            assert_eq!(REGISTRY.snapshot().bytes, before.bytes);
            free(pointer);
        }
    }
    let after = REGISTRY.snapshot();
    assert!(!after.invalid);
    assert_eq!(after.blocks, before.blocks);
    assert_eq!(after.bytes, before.bytes);
    assert!(after.peak >= before.bytes + 8192);
    for ((before, after), expected) in
        count_before
            .into_iter()
            .zip(calls())
            .zip([2, 2, 4, 6 + usize::from(zero_owned), 3, 1])
    {
        assert_eq!(after - before, expected, "outer allocator call count");
    }
    assert!(null_returns() >= null_before + 2);
    std::thread::scope(|scope| {
        let workers: [_; 4] = std::array::from_fn(|_| {
            scope.spawn(|| {
                for _ in 0..256 {
                    // Every thread owns its own nonnull extent until successful
                    // resizing transfers ownership, then releases it exactly once.
                    unsafe {
                        let pointer = malloc(black_box(64));
                        assert!(!pointer.is_null());
                        pointer.cast::<u8>().write_bytes(0x6b, 64);
                        let resized = realloc(pointer, black_box(128));
                        assert!(!resized.is_null());
                        assert!(std::slice::from_raw_parts(resized.cast::<u8>(), 64)
                            .iter()
                            .all(|&b| b == 0x6b));
                        free(resized);
                    }
                }
            })
        });
        // Explicit joins wait for thread-local destructors as well as closures.
        for worker in workers {
            worker.join().unwrap();
        }
    });
    assert!(!REGISTRY.snapshot().invalid);
    if zero_resize {
        // The result may be null after releasing or retaining old storage; this
        // process deliberately abandons ambiguous ownership and rejects evidence.
        unsafe {
            let pointer = malloc(black_box(8));
            assert!(!pointer.is_null());
            let result = realloc(pointer, black_box(0));
            if !result.is_null() {
                free(result);
            }
        }
        assert!(REGISTRY.snapshot().invalid);
    }
}
