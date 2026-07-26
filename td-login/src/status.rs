//! The kernel's own view of this process's credentials, read from
//! `/proc/self/status`.
//!
//! This is not a convenience: it is the post-condition half of the credential
//! switch (THREAT-MODEL.md §2, Layer 2). `creds::apply` re-reads it after
//! `setgroups`/`setgid`/`setuid` and refuses to `exec` unless the kernel agrees
//! with what was asked for, so a partially applied switch cannot reach a shell.
//! It is also how td-login learns its OWN uid — safe `std` exposes no
//! `getuid(2)`, and adding one to the syscall roster to learn something this
//! file already has to report would widen the surface for nothing.

use std::fs;

/// `/proc/self/status`'s four-column credential fields, in the kernel's order.
pub const REAL: usize = 0;
pub const EFFECTIVE: usize = 1;
pub const SAVED: usize = 2;
pub const FILESYSTEM: usize = 3;

/// One process's credential state. Every field is required; a `/proc` that
/// cannot answer all of them is a `/proc` this crate will not switch under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// real, effective, saved, filesystem.
    pub uid: [u32; 4],
    /// real, effective, saved, filesystem.
    pub gid: [u32; 4],
    /// The supplementary set, sorted so it compares as a set rather than a list.
    pub groups: Vec<u32>,
    /// Thread count. The raw syscalls are per-thread (THREAT-MODEL.md §2), so
    /// anything but 1 makes a switch unverifiable.
    pub threads: u32,
    /// Permitted, effective, ambient and inheritable capability sets.
    ///
    /// A capability is a credential too, and one the uid columns cannot show. The
    /// kernel normally clears the permitted and effective sets when every uid
    /// leaves 0 (`cap_emulate_setxuid`), but `SECBIT_NO_SETUID_FIXUP` — unlike
    /// `SECBIT_KEEP_CAPS`, it survives `execve` — turns that off, and then
    /// `setuid(1000)` leaves a full `CapEff` behind with every uid, gid and group
    /// reading exactly right. Ambient is the one that would then also survive the
    /// exec into the user's shell.
    pub cap_prm: u64,
    pub cap_eff: u64,
    pub cap_amb: u64,
    /// Parsed but deliberately NOT part of the post-condition: it grants nothing
    /// without a file capability on the next `execve` (td ships none), and it is
    /// routinely non-zero. See `Credentials::matches`.
    pub cap_inh: u64,
}

impl Status {
    pub fn read() -> Result<Status, String> {
        Self::read_from("/proc/self/status")
    }

    pub fn read_from(path: &str) -> Result<Status, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("cannot read {path}: {e} (is /proc mounted?)"))?;
        Self::parse(&text).map_err(|e| format!("{path}: {e}"))
    }

    /// Split out from `read_from` so the parse — the part with judgement in it —
    /// is reachable from a test without a live `/proc`.
    pub fn parse(text: &str) -> Result<Status, String> {
        let mut uid = None;
        let mut gid = None;
        let mut groups = None;
        let mut threads = None;
        let mut cap_prm = None;
        let mut cap_eff = None;
        let mut cap_amb = None;
        let mut cap_inh = None;
        for line in text.lines() {
            let Some((key, rest)) = split_field(line) else {
                continue;
            };
            match key {
                "Uid" => uid = Some(quad(key, rest)?),
                "Gid" => gid = Some(quad(key, rest)?),
                // The kernel writes `Groups:` with a trailing space and no
                // entries when there are none, so an empty list is legitimate
                // here — unlike the list handed to `setgroups(2)`.
                "Groups" => {
                    let mut list = Vec::new();
                    for token in rest.split_whitespace() {
                        list.push(number("Groups", token)?);
                    }
                    list.sort_unstable();
                    list.dedup();
                    groups = Some(list);
                }
                "Threads" => threads = Some(number(key, rest.trim())?),
                "CapPrm" => cap_prm = Some(capability(key, rest.trim())?),
                "CapEff" => cap_eff = Some(capability(key, rest.trim())?),
                "CapAmb" => cap_amb = Some(capability(key, rest.trim())?),
                "CapInh" => cap_inh = Some(capability(key, rest.trim())?),
                _ => {}
            }
        }
        // Absent, not defaulted: every one of these is load-bearing, and a
        // missing line means this is not the file we think it is.
        let (
            Some(uid),
            Some(gid),
            Some(groups),
            Some(threads),
            Some(cap_prm),
            Some(cap_eff),
            Some(cap_amb),
            Some(cap_inh),
        ) = (uid, gid, groups, threads, cap_prm, cap_eff, cap_amb, cap_inh)
        else {
            return Err(
                "missing one of the Uid/Gid/Groups/Threads/CapPrm/CapEff/CapAmb/CapInh \
                 lines"
                    .into(),
            );
        };
        Ok(Status {
            uid,
            gid,
            groups,
            threads,
            cap_prm,
            cap_eff,
            cap_amb,
            cap_inh,
        })
    }

    /// The effective uid — what the kernel checks for `CAP_SETUID`/`CAP_SETGID`.
    pub fn is_root(&self) -> bool {
        self.uid.get(EFFECTIVE) == Some(&0)
    }
}

/// `"Uid:\t0\t0\t0\t0"` -> `("Uid", "0\t0\t0\t0")`. Returns `None` for a line
/// with no `:`, which /proc does not emit but a fixture might.
fn split_field(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once(':')?;
    Some((key, rest))
}

/// A `Cap*:` line — 16 hex digits, no `0x`.
fn capability(key: &str, token: &str) -> Result<u64, String> {
    u64::from_str_radix(token, 16)
        .map_err(|_| format!("{key}: {token:?} is not a hexadecimal capability set"))
}

fn number(key: &str, token: &str) -> Result<u32, String> {
    token
        .parse::<u32>()
        .map_err(|_| format!("{key}: {token:?} is not an unsigned 32-bit id"))
}

/// The four whitespace-separated ids of a `Uid:`/`Gid:` line. Exactly four —
/// a shorter line would leave a field defaulted, and the defaulted one would be
/// the one nobody checked.
fn quad(key: &str, rest: &str) -> Result<[u32; 4], String> {
    let mut ids = [0u32; 4];
    let mut seen = 0usize;
    for token in rest.split_whitespace() {
        let Some(slot) = ids.get_mut(seen) else {
            return Err(format!("{key}: more than four ids"));
        };
        *slot = number(key, token)?;
        seen += 1;
    }
    if seen != 4 {
        return Err(format!("{key}: expected four ids, got {seen}"));
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// A verbatim excerpt of a real `/proc/self/status`, tabs and the trailing
    /// space on `Groups:` included. Written as an explicit string rather than
    /// read from the runner's own /proc so the test asserts a known answer.
    const SAMPLE: &str = "Name:\tsh\n\
         Umask:\t0022\n\
         State:\tS (sleeping)\n\
         Tgid:\t412\n\
         Pid:\t412\n\
         PPid:\t411\n\
         TracerPid:\t0\n\
         Uid:\t1000\t1000\t1000\t1000\n\
         Gid:\t1000\t1000\t1000\t1000\n\
         FDSize:\t64\n\
         Groups:\t1000 10 \n\
         Threads:\t1\n\
         SigQ:\t0/3654\n\
         CapInh:\t0000000000000000\n\
         CapPrm:\t0000000000000000\n\
         CapEff:\t0000000000000000\n\
         CapBnd:\t000001ffffffffff\n\
         CapAmb:\t0000000000000000\n";

    #[test]
    fn a_real_status_parses_into_all_four_fields() {
        let s = Status::parse(SAMPLE).unwrap();
        assert_eq!(s.uid, [1000; 4]);
        assert_eq!(s.gid, [1000; 4]);
        // Sorted on the way in, so a set comparison downstream is just `==`.
        assert_eq!(s.groups, vec![10, 1000]);
        assert_eq!(s.threads, 1);
        assert!(!s.is_root());
        assert_eq!((s.cap_prm, s.cap_eff, s.cap_amb, s.cap_inh), (0, 0, 0, 0));
        // CapBnd is a BOUNDING set, not a held privilege, and is full for an
        // ordinary process — reading it as one would refuse every real session.
        let full = SAMPLE.replace("CapPrm:\t0000000000000000", "CapPrm:\t000001ffffffffff");
        assert_eq!(Status::parse(&full).unwrap().cap_prm, 0x0000_01ff_ffff_ffff);
    }

    #[test]
    fn root_is_recognised_by_its_effective_uid() {
        let text = SAMPLE.replace("Uid:\t1000\t1000\t1000\t1000", "Uid:\t0\t0\t0\t0");
        assert!(Status::parse(&text).unwrap().is_root());
        // A process that has dropped its EFFECTIVE uid but kept the saved one is
        // NOT root for our purposes: the kernel checks the effective id, and
        // reading any other column here would call a dropped process privileged.
        let dropped = SAMPLE.replace("Uid:\t1000\t1000\t1000\t1000", "Uid:\t0\t1000\t0\t1000");
        assert!(!Status::parse(&dropped).unwrap().is_root());
    }

    #[test]
    fn an_empty_supplementary_set_parses_as_empty() {
        let text = SAMPLE.replace("Groups:\t1000 10 \n", "Groups:\t\n");
        assert_eq!(Status::parse(&text).unwrap().groups, Vec::<u32>::new());
    }

    /// Every absent or malformed field is an error, never a default. A defaulted
    /// `Threads` would read as single-threaded and a defaulted `Groups` as
    /// "no residual groups" — each one a green light for the exact state the
    /// post-condition check exists to catch.
    #[test]
    fn a_missing_or_malformed_field_fails_rather_than_defaulting() {
        for drop_line in [
            "Uid:", "Gid:", "Groups:", "Threads:", "CapPrm:", "CapEff:", "CapAmb:", "CapInh:",
        ] {
            let text: String = SAMPLE
                .lines()
                .filter(|l| !l.starts_with(drop_line))
                .map(|l| format!("{l}\n"))
                .collect();
            assert!(
                Status::parse(&text).is_err(),
                "a status with no {drop_line} line must not parse"
            );
        }
        assert!(Status::parse(&SAMPLE.replace("\t1000\t1000\t1000\t1000", "\t1000\t1000")).is_err());
        assert!(Status::parse(&SAMPLE.replace("Threads:\t1", "Threads:\tmany")).is_err());
        assert!(Status::parse(&SAMPLE.replace("Groups:\t1000 10 ", "Groups:\t1000 -1 ")).is_err());
        assert!(Status::parse(&SAMPLE.replace("CapEff:\t0000000000000000", "CapEff:\tzz")).is_err());
        // Five ids is as wrong as three: it means the format moved under us.
        assert!(Status::parse(&SAMPLE.replace("Gid:\t1000\t1000\t1000\t1000", "Gid:\t1\t1\t1\t1\t1")).is_err());
    }

    /// The live kernel must carry every field the fixture does, with the values
    /// the file itself states.
    ///
    /// The runner's own ids are whatever the runner is, so this cannot assert
    /// constants — but it CAN assert that the parse reproduces the same file it
    /// read, field by field, which is what makes it more than "read() returned
    /// Ok". An earlier revision of this test compared a value to itself and
    /// called `get` on a fixed-size array; both are true by construction.
    #[test]
    fn the_running_kernel_answers_in_the_expected_shape() {
        let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
            return; // no /proc (a sandbox); the fixture legs still cover the parse
        };
        let parsed = Status::parse(&text);
        assert!(
            parsed.is_ok(),
            "the live /proc/self/status did not parse: {parsed:?}"
        );
        let s = parsed.unwrap();
        // Pull the same fields out a second, independent way and compare.
        let field = |name: &str| -> Vec<String> {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix(name) {
                    return rest.split_whitespace().map(str::to_string).collect();
                }
            }
            Vec::new()
        };
        let uid = field("Uid:");
        assert_eq!(uid.len(), 4, "the kernel's Uid: line is no longer four columns");
        for (column, seen) in [REAL, EFFECTIVE, SAVED, FILESYSTEM].iter().zip(uid.iter()) {
            assert_eq!(s.uid.get(*column).map(u32::to_string).as_deref(), Some(seen.as_str()));
        }
        assert_eq!(field("Gid:").len(), 4);
        assert_eq!(field("Threads:").first().map(String::as_str), Some(s.threads.to_string().as_str()));
        assert!(s.threads >= 1, "a live process has at least one thread");
        // Groups are sorted on the way in, whatever order the kernel printed.
        let mut raw: Vec<u32> = field("Groups:").iter().filter_map(|g| g.parse().ok()).collect();
        raw.sort_unstable();
        raw.dedup();
        assert_eq!(s.groups, raw);
        // An unprivileged runner holds no capabilities; a root one may.
        // Permitted/effective/ambient are empty for an unprivileged process.
        // Inheritable is NOT asserted: it is commonly non-zero (this runner's own
        // is 0x800000000) and `Credentials::matches` deliberately ignores it.
        if !s.is_root() {
            assert_eq!((s.cap_prm, s.cap_eff, s.cap_amb), (0, 0, 0));
        }
    }
}
