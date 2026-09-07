//! Join the UID-selected unprivileged session leaf before dropping root credentials.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

const SESSION_PROCS: &str = "/sys/fs/cgroup/td-user-1000/session/cgroup.procs";
const SESSION_MEMBERSHIP: &str = "0::/td-user-1000/session";
const MAX_CGROUP_BYTES: u64 = 4096;

pub(crate) fn join(uid: u32) -> Result<(), String> {
    if uid == 0 {
        return Ok(());
    }
    let (procs, expected) = session_paths(uid)?;
    if current_membership()? == expected {
        return Ok(());
    }
    let pid = std::process::id().to_string();
    let mut file = OpenOptions::new()
        .write(true)
        .open(&procs)
        .map_err(|error| format!("cannot open {procs}: {error}"))?;
    let command = format!("{pid}\n");
    let written = file
        .write(command.as_bytes())
        .map_err(|error| format!("cannot join {procs}: {error}"))?;
    if written != command.len() {
        return Err(format!(
            "cannot join {procs}: wrote {written} of {} bytes",
            command.len()
        ));
    }
    let actual = current_membership()?;
    if actual != expected {
        return Err(format!(
            "session cgroup read back as {actual:?}, expected {expected:?}"
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

fn session_paths(uid: u32) -> Result<(String, String), String> {
    let component = delegation(uid)?;
    if uid == 1000 {
        return Ok((SESSION_PROCS.into(), SESSION_MEMBERSHIP.into()));
    }
    Ok((
        format!("/sys/fs/cgroup/{component}/session/cgroup.procs"),
        format!("0::/{component}/session"),
    ))
}

fn delegation(uid: u32) -> Result<String, String> {
    match uid {
        1000 => Ok("td-user-1000".into()),
        65536..=2147483647 => Ok(format!("td-app-{uid}")),
        _ => Err(format!(
            "no delegated session cgroup is configured for uid {uid}"
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn session_control_paths_and_membership_match_the_supervisors_contract() {
        assert_eq!(
            session_paths(1000).unwrap(),
            (
                "/sys/fs/cgroup/td-user-1000/session/cgroup.procs".into(),
                "0::/td-user-1000/session".into()
            )
        );
        assert_eq!(
            session_paths(65536).unwrap(),
            (
                "/sys/fs/cgroup/td-app-65536/session/cgroup.procs".into(),
                "0::/td-app-65536/session".into()
            )
        );
    }

    #[test]
    fn human_and_application_sessions_have_disjoint_delegations() {
        assert_eq!(delegation(1000).unwrap(), "td-user-1000");
        for uid in [65536, 65537, 2147483647] {
            assert_eq!(delegation(uid).unwrap(), format!("td-app-{uid}"));
        }
        for uid in [0, 991, 1001, 65533, 65534, 65535, 2147483648, u32::MAX] {
            assert!(delegation(uid).is_err());
        }
    }
}
