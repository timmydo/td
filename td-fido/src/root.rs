//! The root console's admission, shared with td-secret's store, which
//! compiles this file by path (td-fido/DESIGN.md, "Scope").

use std::fs;

/// Every user ID of this process is 0.
pub fn require_root() -> Result<(), String> {
    let status = fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
    check_root(&status)
}

pub(crate) fn check_root(status: &str) -> Result<(), String> {
    let ids = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .ok_or("missing process credentials")?
        .split_whitespace()
        .collect::<Vec<_>>();
    if ids != ["0", "0", "0", "0"] {
        return Err("TPM enrollment and release require the root console".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_admission_requires_every_user_id_zero() {
        assert_eq!(check_root("Name:\tx\nUid:\t0\t0\t0\t0\nGid:\t5\n"), Ok(()));
        for uids in ["0\t0\t0\t1", "1000\t0\t0\t0", "0\t0\t0", "00\t0\t0\t0"] {
            assert_eq!(
                check_root(&format!("Uid:\t{uids}\n")),
                Err("TPM enrollment and release require the root console".into()),
                "{uids}"
            );
        }
        for status in [
            "",
            "Name:\tx\n",
            "Uid: 1000 0 0 0",
            "Uid: 0 0 0",
            "Uid: 0 0 0 0 0",
        ] {
            assert!(check_root(status).is_err(), "{status}");
        }
    }
}
