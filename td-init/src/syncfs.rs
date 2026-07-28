//! `sync` — flush every filesystem, as `/etc/shutdown` does before unmounting.
//!
//! Here rather than in td-util because `sync(2)` is a SYSCALL, and td-util
//! `forbid`s unsafe. It is already one of this crate's nine — `halt` calls it
//! before every power-down — so serving it as an applet widens no surface: same
//! syscall, same wrapper, one more caller.

use crate::sys;

pub fn run(args: &[String]) -> Result<u8, String> {
    // No operands. The `sync FILE` form flushes only that file's filesystem;
    // accepting and ignoring a path would report a whole-system flush that did
    // not happen, on the one path where the next step is cutting power.
    if let Some(a) = args.first() {
        return Err(format!("unexpected argument '{a}'\nusage: sync"));
    }
    sys::sync();
    Ok(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// It really flushes, and says so with 0.
    #[test]
    fn sync_takes_no_operands_and_succeeds() {
        assert_eq!(run(&[]), Ok(0));
        assert!(
            run(&["/var".to_string()]).is_err(),
            "a path operand must be refused, not silently ignored"
        );
    }
}
