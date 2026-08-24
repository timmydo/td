//! Join the fixed unprivileged session leaf before dropping root credentials.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

const APPLICATION_UID: u32 = 1000;
const SESSION_PROCS: &str = "/sys/fs/cgroup/td-user-1000/session/cgroup.procs";
const SESSION_MEMBERSHIP: &str = "0::/td-user-1000/session";
const MAX_CGROUP_BYTES: u64 = 4096;

pub(crate) fn join(uid: u32) -> Result<(), String> {
    if uid == 0 {
        return Ok(());
    }
    if uid != APPLICATION_UID {
        return Err(format!(
            "no delegated session cgroup is configured for uid {uid}"
        ));
    }
    if current_membership()? == SESSION_MEMBERSHIP {
        return Ok(());
    }
    let pid = std::process::id().to_string();
    let mut file = OpenOptions::new()
        .write(true)
        .open(SESSION_PROCS)
        .map_err(|error| format!("cannot open {SESSION_PROCS}: {error}"))?;
    let command = format!("{pid}\n");
    let written = file
        .write(command.as_bytes())
        .map_err(|error| format!("cannot join {SESSION_PROCS}: {error}"))?;
    if written != command.len() {
        return Err(format!(
            "cannot join {SESSION_PROCS}: wrote {written} of {} bytes",
            command.len()
        ));
    }
    let actual = current_membership()?;
    if actual != SESSION_MEMBERSHIP {
        return Err(format!(
            "session cgroup read back as {actual:?}, expected {SESSION_MEMBERSHIP:?}"
        ));
    }
    Ok(())
}

fn current_membership() -> Result<String, String> {
    read_bounded(Path::new("/proc/self/cgroup"))
}

fn read_bounded(path: &Path) -> Result<String, String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?
        .take(MAX_CGROUP_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_CGROUP_BYTES {
        return Err(format!(
            "{} exceeds {MAX_CGROUP_BYTES} bytes",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map(|text| text.trim().to_string())
        .map_err(|error| format!("{} is not UTF-8: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_membership_is_one_fixed_unified_row() {
        assert_eq!(SESSION_MEMBERSHIP, "0::/td-user-1000/session");
        assert_eq!(
            SESSION_PROCS,
            "/sys/fs/cgroup/td-user-1000/session/cgroup.procs"
        );
    }
}
