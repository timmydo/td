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

/// The runtime account td-app's runtime verbs run as
/// (`flathub_app::RUNTIME_UID`), and its delegation, which td-firstboot
/// prepares; the recipes hold the three crates to one value.
const RUNTIME_UID: u32 = 998;
const RUNTIME_DELEGATION: &str = "td-runtimes";

/// The delegation `uid`'s session joins. A workload account's is its own,
/// by the ledger's range its uid lies in: `td-app-UID` for an application,
/// `td-workload-UID` for a sandbox or workspace.
fn delegation(uid: u32) -> Result<String, String> {
    match uid {
        1000 => Ok("td-user-1000".into()),
        RUNTIME_UID => Ok(RUNTIME_DELEGATION.into()),
        uid => match crate::workload_accounts::delegation(uid) {
            Some(name) => Ok(format!("{name}")),
            None => Err(format!(
                "no delegated session cgroup is configured for uid {uid}"
            )),
        },
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
        for uid in [65536, 65537, 98303] {
            assert_eq!(delegation(uid).unwrap(), format!("td-app-{uid}"));
        }
        // A sandbox or workspace account has a delegation of its own, no
        // application's.
        for uid in [98304, 100000, 131071] {
            assert_eq!(delegation(uid).unwrap(), format!("td-workload-{uid}"));
            assert_eq!(
                session_paths(uid).unwrap(),
                (
                    format!("/sys/fs/cgroup/td-workload-{uid}/session/cgroup.procs"),
                    format!("0::/td-workload-{uid}/session")
                )
            );
        }
        for uid in [
            0,
            991,
            997,
            999,
            1001,
            65533,
            65534,
            65535,
            131072,
            2147483647,
            2147483648,
            u32::MAX,
        ] {
            assert!(delegation(uid).is_err());
        }
    }

    /// The runtime account's td-app lands in a delegation of its own, not
    /// in root's request cgroup and not in any application's.
    #[test]
    fn the_runtime_account_has_its_own_delegation() {
        assert_eq!(delegation(RUNTIME_UID).unwrap(), "td-runtimes");
        assert_eq!(
            session_paths(RUNTIME_UID).unwrap(),
            (
                "/sys/fs/cgroup/td-runtimes/session/cgroup.procs".into(),
                "0::/td-runtimes/session".into()
            )
        );
    }
}
