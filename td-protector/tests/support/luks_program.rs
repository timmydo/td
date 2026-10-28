//! Explicit td-luks executable for the opt-in program tests.
use std::path::PathBuf;

pub fn path() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("TD_LUKS").expect("set TD_LUKS to the absolute td-luks executable"),
    );
    assert!(
        path.is_absolute() && path.is_file(),
        "TD_LUKS must name an absolute executable file"
    );
    path
}
