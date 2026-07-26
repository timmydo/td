//! The ONE place td-login changes process credentials.
//!
//! THREAT-MODEL.md §2 is the specification for this file. In short: the order is
//! `setgroups` -> `setgid` -> `setuid`, every return value is checked, and the
//! result is then read back out of `/proc/self/status` and compared before any
//! caller is allowed to `exec`. `Credentials` cannot be built without all three
//! of uid, gid and group list, so "forgot the groups" is not a reachable state
//! rather than a mistake to remember not to make.

use crate::status::{Status, EFFECTIVE, FILESYSTEM, REAL, SAVED};
use crate::sys;

/// A complete credential set. The fields are PRIVATE, so `new` below is the only
/// way to build one — it folds the primary gid into the supplementary list and
/// sorts it, and sorting is what makes the post-condition an equality rather than
/// a subset test. A literal built elsewhere could carry an unsorted or
/// gid-less set that `matches` would then compare against the kernel's sorted
/// answer, so the compiler is made to refuse one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
}

impl Credentials {
    pub fn new(uid: u32, gid: u32, supplementary: &[u32]) -> Credentials {
        let mut groups = Vec::with_capacity(supplementary.len() + 1);
        groups.extend_from_slice(supplementary);
        // The primary gid is always in the set. Linux's own `initgroups(3)` does
        // this, and leaving it out would make the readback disagree with the
        // kernel for reasons that have nothing to do with a failed switch.
        groups.push(gid);
        groups.sort_unstable();
        groups.dedup();
        Credentials { uid, gid, groups }
    }

    /// Does the kernel's view equal what was asked for? ALL FOUR uid columns and
    /// ALL FOUR gid columns, not just the effective one: a switch that left a
    /// saved uid of 0 behind is a process one `setuid(0)` away from being root
    /// again, which is the escalation this whole file is arranged around.
    pub fn matches(&self, seen: &Status) -> Result<(), String> {
        for (column, name) in [
            (REAL, "real"),
            (EFFECTIVE, "effective"),
            (SAVED, "saved"),
            (FILESYSTEM, "filesystem"),
        ] {
            if seen.uid.get(column) != Some(&self.uid) {
                return Err(format!(
                    "{name} uid is {:?}, expected {}",
                    seen.uid.get(column),
                    self.uid
                ));
            }
            if seen.gid.get(column) != Some(&self.gid) {
                return Err(format!(
                    "{name} gid is {:?}, expected {}",
                    seen.gid.get(column),
                    self.gid
                ));
            }
        }
        if seen.groups != self.groups {
            return Err(format!(
                "supplementary groups are {:?}, expected {:?}",
                seen.groups, self.groups
            ));
        }
        // A capability is a credential the uid columns cannot show. The kernel
        // clears the permitted and effective sets when every uid leaves 0 — but
        // only with the default securebits: `SECBIT_NO_SETUID_FIXUP` turns that
        // off and survives `execve`, so an ancestor could hand td-login a state
        // in which `setuid(1000)` produces a perfect four-column readback and a
        // full `CapEff`. Non-root means non-root here too.
        //
        // Permitted, effective and ambient — NOT inheritable, and not the
        // bounding set. Inheritable survives the drop but grants nothing until
        // an `execve` of a file that carries inheritable file capabilities, and
        // td ships none (they are xattrs; NAR does not carry them). It is also
        // routinely non-zero for ordinary processes, so requiring it empty would
        // refuse legitimate sessions to defend against a conversion that cannot
        // happen here. Ambient is the set that needs no file capability, and it
        // IS checked.
        if self.uid != 0 {
            for (held, name) in [
                (seen.cap_prm, "permitted"),
                (seen.cap_eff, "effective"),
                (seen.cap_amb, "ambient"),
            ] {
                if held != 0 {
                    return Err(format!(
                        "uid {} still holds {name} capabilities {held:#018x} \
                         (SECBIT_NO_SETUID_FIXUP?)",
                        self.uid
                    ));
                }
            }
        }
        Ok(())
    }
}

/// May this process change credentials at all? Split out of `apply` so it is
/// reachable from a test: `apply` reads the live `/proc`, and a rule about
/// process states this one cannot be in is otherwise only assertable by
/// inspection.
fn may_switch(before: &Status) -> Result<(), String> {
    // EVERY uid column, not the effective one. THREAT-MODEL.md §4 says td-login
    // is never installed setuid-root, and this is what turns that from a
    // packaging promise into a refusal: under a setuid-root exec the real uid
    // stays the caller's while the effective one is 0, so an "is the effective
    // uid 0" gate would let an unprivileged caller through — and `su` takes the
    // forced policy path, so they would reach root without authenticating. The
    // recipe's shape check asserts the shipped binary carries no setuid bit;
    // this asserts it a second way, at the moment it would matter.
    if before.uid != [0; 4] {
        return Err(format!(
            "only root may switch credentials (uid columns are {:?}); td-login is \
             deliberately not installed setuid-root, and a process whose uid \
             columns disagree was entered through something that is",
            before.uid
        ));
    }
    // The raw syscalls change the CALLING THREAD's credentials, where glibc's
    // wrappers broadcast to every thread. td-login starts none, so this is a
    // tripwire for a future change rather than a live condition.
    if before.threads != 1 {
        return Err(format!(
            "refusing to switch credentials in a {}-threaded process: the raw \
             setuid/setgid/setgroups syscalls apply to this thread only",
            before.threads
        ));
    }
    Ok(())
}

/// Apply `want` to THIS process, then prove it took.
///
/// Order is the contract (THREAT-MODEL.md §2): supplementary groups first,
/// because `setgroups(2)` needs the `CAP_SETGID` that `setuid(2)` is about to
/// remove; the primary group second; the uid last. Each return value is checked,
/// so a failure stops here instead of continuing with a half-changed identity.
///
/// The readback afterwards is the part that does not depend on having reasoned
/// about the order correctly. Whatever the kernel says is what the caller gets,
/// and if it is not what was asked for, nobody execs anything.
pub fn apply(want: &Credentials) -> Result<(), String> {
    let before = Status::read()?;
    // Already there: `su` to yourself, or a `login -f` for the user we already
    // are. Nothing to change, and nothing to be privileged for.
    if want.matches(&before).is_ok() {
        return Ok(());
    }
    may_switch(&before)?;

    sys::setgroups(&want.groups)
        .map_err(|e| format!("setgroups({:?}) failed: {e}", want.groups))?;
    sys::setgid(want.gid).map_err(|e| format!("setgid({}) failed: {e}", want.gid))?;
    sys::setuid(want.uid).map_err(|e| format!("setuid({}) failed: {e}", want.uid))?;

    let after = Status::read()?;
    want.matches(&after).map_err(|e| {
        format!("credential switch did not take effect: {e}; refusing to start a session")
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn status(uid: [u32; 4], gid: [u32; 4], groups: &[u32]) -> Status {
        Status {
            uid,
            gid,
            groups: groups.to_vec(),
            threads: 1,
            cap_prm: 0,
            cap_eff: 0,
            cap_amb: 0,
            cap_inh: 0,
        }
    }

    #[test]
    fn the_primary_gid_is_always_in_the_supplementary_set() {
        assert_eq!(Credentials::new(1000, 1000, &[10]).groups, vec![10, 1000]);
        assert_eq!(Credentials::new(0, 0, &[]).groups, vec![0]);
        // Sorted and deduplicated, so the readback comparison is an equality and
        // the order /etc/group happened to list them in cannot matter.
        assert_eq!(
            Credentials::new(1000, 1000, &[1000, 10, 10, 5]).groups,
            vec![5, 10, 1000]
        );
    }

    /// A four-column readback can be perfect and the process still root in the
    /// way that counts. With `SECBIT_NO_SETUID_FIXUP` set by an ancestor the
    /// kernel does not clear the capability sets on the way down, and ambient
    /// ones survive the exec into the user's shell.
    #[test]
    fn a_capability_left_behind_fails_the_match() {
        let want = Credentials::new(1000, 1000, &[10]);
        let ok = status([1000; 4], [1000; 4], &[10, 1000]);
        assert!(want.matches(&ok).is_ok());
        for (field, name) in [
            (0usize, "permitted"),
            (1, "effective"),
            (2, "ambient"),
        ] {
            let mut seen = ok.clone();
            match field {
                0 => seen.cap_prm = 0x0000_01ff_ffff_ffff,
                1 => seen.cap_eff = 1 << 21, // CAP_SYS_ADMIN alone is enough
                _ => seen.cap_amb = 1 << 7,  // CAP_SETUID
            }
            let err = want.matches(&seen).unwrap_err();
            assert!(err.contains(name), "{name} capabilities must fail the match, got: {err}");
        }
        // Inheritable is deliberately NOT enforced: see the comment in `matches`.
        // A non-zero one is ordinary — the machine this was written on hands every
        // process CapInh 0x800000000 — so requiring it empty would refuse real
        // sessions.
        let mut seen = ok.clone();
        seen.cap_inh = 0x0000_0008_0000_0000;
        assert!(want.matches(&seen).is_ok());
        // Becoming root legitimately (su root, login -f root) is not a residue.
        let root = Credentials::new(0, 0, &[]);
        let mut seen = status([0; 4], [0; 4], &[0]);
        seen.cap_prm = 0x0000_01ff_ffff_ffff;
        seen.cap_eff = 0x0000_01ff_ffff_ffff;
        assert!(root.matches(&seen).is_ok());
    }

    /// The four-column check. Each of these is a switch that "worked" by the
    /// effective uid alone and is nonetheless an escalation.
    #[test]
    fn a_residual_credential_in_any_column_fails_the_match() {
        let want = Credentials::new(1000, 1000, &[10]);
        assert!(want
            .matches(&status([1000; 4], [1000; 4], &[10, 1000]))
            .is_ok());
        // Saved uid still 0 — one setuid(0) from root again.
        assert!(want
            .matches(&status([1000, 1000, 0, 1000], [1000; 4], &[10, 1000]))
            .is_err());
        // Filesystem uid left behind: file access still happens as root.
        assert!(want
            .matches(&status([1000, 1000, 1000, 0], [1000; 4], &[10, 1000]))
            .is_err());
        // The gid never moved.
        assert!(want
            .matches(&status([1000; 4], [0; 4], &[10, 1000]))
            .is_err());
        // The canonical one: uid dropped, root's group set still attached.
        assert!(want
            .matches(&status([1000; 4], [1000; 4], &[0, 10, 1000]))
            .is_err());
        // ...and the mirror, a set SMALLER than asked for.
        assert!(want.matches(&status([1000; 4], [1000; 4], &[1000])).is_err());
    }

    /// The diagnostic names the column, because "credential switch failed" with
    /// no column is a message that sends the reader to the wrong syscall.
    #[test]
    fn a_mismatch_says_which_column_disagreed() {
        let want = Credentials::new(1000, 1000, &[]);
        let err = want
            .matches(&status([1000, 1000, 0, 1000], [1000; 4], &[1000]))
            .unwrap_err();
        assert!(err.contains("saved uid"), "unhelpful diagnostic: {err}");
        let err = want
            .matches(&status([1000; 4], [1000; 4], &[10, 1000]))
            .unwrap_err();
        assert!(err.contains("supplementary groups"), "unhelpful: {err}");
    }

    /// `apply` on an unprivileged runner must refuse rather than attempt: the
    /// early-return is a no-op switch, and anything else is a named error. This
    /// runs as whoever `cargo test` runs as, so it asserts the branch that is
    /// reachable there — and, when that is root, the no-op path.
    /// The never-setuid-root boundary, enforced rather than assumed. Under a
    /// setuid-root exec the real uid stays the caller's and the effective one is
    /// 0, so a gate that asked only about the effective uid would let an
    /// unprivileged caller switch — and `su` takes the forced policy path, so
    /// they would become root without authenticating.
    #[test]
    fn a_process_whose_uid_columns_disagree_may_not_switch() {
        for uid in [
            [1000, 0, 0, 0], // classic setuid-root exec: euid 0, ruid the caller's
            [0, 0, 1000, 0], // a saved uid that is not root
            [0, 0, 0, 1000], // fsuid left behind
            [1000; 4],       // plainly unprivileged
        ] {
            let seen = status(uid, [0; 4], &[0]);
            let err = may_switch(&seen).unwrap_err();
            assert!(err.contains("only root"), "{uid:?} got: {err}");
        }
        // Root in every column proceeds — otherwise the above would pass for a
        // gate that refused everything.
        assert!(may_switch(&status([0; 4], [0; 4], &[0])).is_ok());
        // The effective column ALONE is not the question: this is the shape a
        // setuid-root binary produces, and `is_root()` says yes to it.
        let setuid_exec = status([1000, 0, 0, 0], [0; 4], &[0]);
        assert!(setuid_exec.is_root(), "the fixture must fool an euid-only gate");
        assert!(may_switch(&setuid_exec).is_err());
        // ...and a multi-threaded process is refused whatever its uids are.
        let mut threaded = status([0; 4], [0; 4], &[0]);
        threaded.threads = 2;
        let err = may_switch(&threaded).unwrap_err();
        assert!(err.contains("2-threaded"), "{err}");
    }

    #[test]
    fn apply_refuses_what_it_cannot_verify() {
        let Ok(now) = Status::read() else {
            return; // no /proc in this sandbox
        };
        let same = Credentials {
            uid: now.uid[EFFECTIVE],
            gid: now.gid[EFFECTIVE],
            groups: now.groups.clone(),
        };
        // Asking for exactly what we already have never touches a syscall.
        assert!(apply(&same).is_ok());
        if !now.is_root() {
            let err = apply(&Credentials::new(now.uid[REAL] + 1, now.gid[REAL], &[])).unwrap_err();
            assert!(err.contains("only root"), "unexpected refusal: {err}");
        }
    }
}
