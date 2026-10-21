//! td-link-test's cdylib. It exports nothing, which would take an unsafe
//! `#[no_mangle]`; the dynamic probe needs it by path, so the loader maps
//! and relocates it before the probe runs.
#![forbid(unsafe_code)]

pub fn answer() -> i32 {
    std::panic::catch_unwind(|| 6 * 7).unwrap_or(0)
}
