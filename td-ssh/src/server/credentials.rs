//! The credential switch: setgroups, setgid, setuid, in that order, then a
//! readback of what the kernel actually holds. It follows td-login's
//! `creds.rs` (td-login/THREAT-MODEL.md section 2) and reads status through a
//! descriptor because the pre-auth jail has no `/proc` (DESIGN.md section 4).

use super::login_status::{Status, EFFECTIVE, FILESYSTEM, REAL, SAVED};
use crate::sys;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

const STATUS_PATH: &str = "/proc/self/status";
const MAX_STATUS: u64 = 64 * 1024;

/// Opens the status descriptor. Call it before any `chroot`.
pub fn open_status() -> std::io::Result<File> {
    File::open(STATUS_PATH)
}

fn read_status(file: &mut File) -> Result<Status, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {STATUS_PATH}: {error}"))?;
    let mut text = String::new();
    file.take(MAX_STATUS)
        .read_to_string(&mut text)
        .map_err(|error| format!("read {STATUS_PATH}: {error}"))?;
    Status::parse(&text)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub uid: u32,
    pub gid: u32,
    groups: Vec<u32>,
}

impl Credentials {
    pub fn new(uid: u32, gid: u32, supplementary: &[u32]) -> Self {
        let mut groups = supplementary.to_vec();
        groups.push(gid);
        groups.sort_unstable();
        groups.dedup();
        Self { uid, gid, groups }
    }

    /// All four columns of both ids, the set of groups, and every capability set
    /// that could survive: a saved uid of 0 is one `setuid(0)` from root.
    fn matches(&self, seen: &Status) -> Result<(), String> {
        for (column, name) in [
            (REAL, "real"),
            (EFFECTIVE, "effective"),
            (SAVED, "saved"),
            (FILESYSTEM, "filesystem"),
        ] {
            if seen.uid.get(column) != Some(&self.uid) {
                return Err(format!("{name} uid is {:?}", seen.uid.get(column)));
            }
            if seen.gid.get(column) != Some(&self.gid) {
                return Err(format!("{name} gid is {:?}", seen.gid.get(column)));
            }
        }
        if seen.groups != self.groups {
            return Err(format!("supplementary groups are {:?}", seen.groups));
        }
        if self.uid != 0 {
            for (held, name) in [
                (seen.cap_prm, "permitted"),
                (seen.cap_eff, "effective"),
                (seen.cap_amb, "ambient"),
            ] {
                if held != 0 {
                    return Err(format!("{name} capabilities {held:#018x} remain"));
                }
            }
        }
        Ok(())
    }
}

/// Refuses unless every uid column is 0 and no other thread exists: the raw calls
/// change only the calling thread.
fn may_switch(before: &Status) -> Result<(), String> {
    if before.uid != [0; 4] {
        return Err(format!(
            "only root may switch credentials (uid columns are {:?})",
            before.uid
        ));
    }
    if before.threads != 1 {
        return Err(format!(
            "refusing to switch credentials in a {}-threaded process",
            before.threads
        ));
    }
    Ok(())
}

/// Switches this process, or fails leaving nothing to run with: every caller
/// exits on an error. The status descriptor stays open for the caller to close.
pub fn switch(want: &Credentials, status: &mut File) -> Result<(), String> {
    let before = read_status(status)?;
    may_switch(&before)?;
    sys::setgroups(&want.groups).map_err(|error| format!("setgroups: {error}"))?;
    sys::setgid(want.gid).map_err(|error| format!("setgid: {error}"))?;
    sys::setuid(want.uid).map_err(|error| format!("setuid: {error}"))?;
    let after = read_status(status)?;
    want.matches(&after)
        .map_err(|error| format!("credential switch did not take effect: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn status(uid: u32, gid: u32, groups: &[u32]) -> Status {
        let quad = |value: u32| format!("{value}\t{value}\t{value}\t{value}");
        let list: Vec<String> = groups.iter().map(u32::to_string).collect();
        let text = format!(
            "Uid:\t{}\nGid:\t{}\nGroups:\t{}\nThreads:\t1\nCapInh:\t0000000000000000\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\nCapAmb:\t0000000000000000\n",
            quad(uid),
            quad(gid),
            list.join(" ")
        );
        Status::parse(&text).unwrap()
    }

    #[test]
    fn readback_compares_every_column_and_the_group_set() {
        let want = Credentials::new(1000, 1000, &[5, 7]);
        assert!(want.matches(&status(1000, 1000, &[5, 7, 1000])).is_ok());
        assert!(want.matches(&status(0, 1000, &[5, 7, 1000])).is_err());
        assert!(want.matches(&status(1000, 0, &[5, 7, 1000])).is_err());
        assert!(want.matches(&status(1000, 1000, &[5, 1000])).is_err());
        let mut saved_root = status(1000, 1000, &[5, 7, 1000]);
        saved_root.uid = [1000, 1000, 0, 1000];
        assert!(want.matches(&saved_root).is_err());
        let mut capable = status(1000, 1000, &[5, 7, 1000]);
        capable.cap_eff = 1;
        assert!(want.matches(&capable).is_err());
    }

    #[test]
    fn only_a_single_threaded_root_may_switch() {
        assert!(may_switch(&status(0, 0, &[])).is_ok());
        assert!(may_switch(&status(1000, 1000, &[])).is_err());
        let mut threaded = status(0, 0, &[]);
        threaded.threads = 2;
        assert!(may_switch(&threaded).is_err());
    }

    #[test]
    fn the_status_descriptor_rereads_after_seeking() {
        let mut file = open_status().unwrap();
        let first = read_status(&mut file).unwrap();
        let second = read_status(&mut file).unwrap();
        assert_eq!(first.uid, second.uid);
    }
}
