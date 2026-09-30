//! A process's identity as `/proc/<pid>/status` states it. Shared source: the
//! compositor's terminal-authority probe and td-term's account lookup (through
//! td-ui) read the same field with this one function.

/// The effective uid, the second field of the status file's `Uid:` line. The
/// effective one is what the kernel checks and what owns `/run/user/UID`.
pub fn effective_uid(status: &str) -> Result<u32, String> {
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .ok_or_else(|| "process status has no Uid line".to_string())?;
    let mut fields = line.split_whitespace();
    let _real = fields
        .next()
        .ok_or_else(|| "process status Uid line has no real uid".to_string())?;
    let effective = fields
        .next()
        .ok_or_else(|| "process status Uid line has no effective uid".to_string())?;
    effective
        .parse()
        .map_err(|_| format!("process status effective uid '{effective}' is not a number"))
}

#[cfg(test)]
mod tests {
    use super::effective_uid;

    #[test]
    fn the_effective_uid_is_the_second_field_of_the_uid_line() {
        let status = "Name:\ttd-term\nUid:\t0\t1000\t1000\t1000\nGid:\t0\t1000\t1000\t1000\n";
        assert_eq!(effective_uid(status).unwrap(), 1000);
        assert!(effective_uid("Name:\ttd-term\n").is_err());
        assert!(effective_uid("Uid:\t1000\n").is_err());
        assert!(effective_uid("Uid:\t1000\tnope\n").is_err());
    }
}
