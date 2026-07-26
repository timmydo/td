//! The `/proc` layer: process identity and process-group/session membership.
//!
//! DESIGN.md I3 — liveness is read from here, never inferred from an exit
//! status. `kill -0` answers through an exit code, so a spawn failure, an
//! ENOENT, or a rejected argv is indistinguishable from ESRCH; a liveness test
//! whose failure mode is "not running" would declare a live service dead and
//! let the teardown unmount underneath it. Reading `/proc` fails CLOSED: an
//! unreadable `/proc` is an error, not an emptiness.

use std::io;

const PROC: &str = "/proc";

/// `ESRCH`. `/proc/<pid>/stat` is a seq_file: if the process exits between the
/// `open` and the `read`, the READ fails with ESRCH rather than the open
/// failing with ENOENT. std maps errno 3 to no named `ErrorKind`, so it arrives
/// as `Uncategorized` and matching on `NotFound` alone misses it — precisely
/// during a teardown, when processes exit fastest and the scan runs most.
const ESRCH: i32 = 3;

/// What identifies a service's processes for the stop path.
///
/// Deliberately ONE of these, never a union. A unit td-svc put in its own
/// process group still sits in td-svc's SESSION, so matching either field would
/// select every other service and the supervisor's own parent. A unit that
/// `setsid()`s (a `tty=` greeter) leaves its group behind the moment it does,
/// and only the session still identifies the login tree once the shell starts
/// making job-control groups inside it.
///
/// The rule that ties them together: whichever is chosen must be one td-svc is
/// NOT itself in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    /// This one process and nothing else.
    ///
    /// The answer for a `tty=` unit that has not `setsid()` yet: it inherited
    /// td-svc's process group AND session, so BOTH of those name the
    /// supervisor. Widening to either would make a request to stop one service
    /// a request to stop the machine.
    Process(i32),
    /// Every process in this process group. `kill(2)` can address it directly.
    Group(i32),
    /// Every process in this session. POSIX has no kill-by-session primitive,
    /// so membership is enumerated — the `killall5` shape.
    Session(i32),
}

/// The three `/proc/<pid>/stat` fields this crate reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Field 5. The process group a signal to `-pgid` would reach.
    pub pgrp: i32,
    /// Field 6. The session — what a `setsid()` child moves into, and the only
    /// thing that still identifies a login tree after it does.
    pub session: i32,
    /// Field 22, in clock ticks since boot. Distinguishes a live pid from a
    /// recycled one; a pid alone cannot.
    pub starttime: u64,
}

/// Parse the fields out of one `/proc/<pid>/stat`.
///
/// `comm` (field 2) is parenthesised and may contain spaces AND `)` — a
/// process can name itself `((evil) thing)`. Splitting the whole line on
/// whitespace therefore misaligns every later field, which is the classic bug
/// here. Everything after the LAST `)` is the fixed-width tail, whose first
/// entry is field 3, so field N lives at tail index N-3.
pub fn parse_stat(text: &str) -> Option<Stat> {
    let close = text.rfind(')')?;
    let tail = text.get(close + 1..)?;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    Some(Stat {
        pgrp: fields.get(2)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        starttime: fields.get(19)?.parse().ok()?,
    })
}

/// Does this error mean "that process is gone"? ENOENT from the open and ESRCH
/// from the read are the two ways the kernel says so; everything else (EACCES,
/// EIO, a `/proc` that is not mounted) is a fault and must propagate.
fn is_gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(ESRCH)
}

/// Read one process's stat. `Ok(None)` means the process is gone, which is a
/// normal answer; any other failure is an error, never an absence.
pub fn stat_of(pid: i32) -> io::Result<Option<Stat>> {
    let path = format!("{PROC}/{pid}/stat");
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse_stat(&text) {
            Some(stat) => Ok(Some(stat)),
            // A stat we cannot parse is NOT "gone": reporting it as absent
            // would be the fail-open shape I3 exists to forbid.
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{path}: unparseable"),
            )),
        },
        Err(e) if is_gone(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Every numeric entry under `/proc`, i.e. every live pid.
fn pids() -> io::Result<Vec<i32>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(PROC)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Ok(pid) = name.parse::<i32>() {
            out.push(pid);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// What one scan of `/proc` found, and what it could not read.
///
/// The two are reported separately because the scan's callers want opposite
/// things from a failure. *Signalling* wants best effort: one unreadable
/// stranger must not stop td-svc TERMing the eight processes it read.
/// *Liveness* (I4, "is this containment empty?") must fail closed: a scan that
/// silently dropped what it could not read would report empty while a service
/// was still running, and the teardown would unmount underneath it. Collapsing
/// either into the other gives one of those two answers wrongly.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub pids: Vec<i32>,
    /// One per `/proc` entry that could not be classified. Non-empty means the
    /// scan is INCOMPLETE, whatever `pids` holds.
    pub errors: Vec<String>,
}

impl Scan {
    /// Is the containment provably empty? Only when nothing was found AND
    /// nothing was unreadable — an incomplete scan proves nothing.
    #[allow(dead_code)]
    pub fn proven_empty(&self) -> bool {
        self.pids.is_empty() && self.errors.is_empty()
    }
}

/// Every pid inside a containment.
///
/// `self_pid` is excluded: td-svc must never signal itself, and a service it
/// did not `setsid()` shares td-svc's own session — so a `Session` query that
/// happened to name it would otherwise return the supervisor.
///
/// Failing to list `/proc` at all is still an `Err`: there is no partial answer
/// to report, and an empty one would be a lie.
pub fn members(mode: Containment, self_pid: i32) -> io::Result<Scan> {
    let mut scan = Scan::default();
    for pid in pids()? {
        if pid == self_pid {
            continue;
        }
        match stat_of(pid) {
            // A process that exits mid-scan is not an error — it is the answer
            // we wanted.
            Ok(None) => {}
            Ok(Some(stat)) => {
                let matches = match mode {
                    Containment::Process(target) => pid == target,
                    Containment::Group(pgid) => stat.pgrp == pgid,
                    Containment::Session(sid) => stat.session == sid,
                };
                if matches {
                    scan.pids.push(pid);
                }
            }
            Err(e) => scan.errors.push(format!("{PROC}/{pid}: {e}")),
        }
    }
    Ok(scan)
}

/// Is this the same process we started, or a pid the kernel recycled?
/// Compares the boot-relative start time, which a new process cannot share.
/// Guards the delayed KILL: `stop-timeout` after a TERM, the pid may belong to
/// something else entirely.
pub fn is_same_process(pid: i32, starttime: u64) -> io::Result<bool> {
    Ok(matches!(stat_of(pid)?, Some(stat) if stat.starttime == starttime))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the kernel actually writes: pid, `(comm)`, state, ppid, pgrp,
    /// session, tty_nr, ... with starttime 22nd.
    fn stat_line(comm: &str, pgrp: i32, session: i32, starttime: u64) -> String {
        let mut fields = vec![format!("7 ({comm}) S 1")];
        // Fields 5..=21, with pgrp and session in their real places.
        fields.push(format!("{pgrp}"));
        fields.push(format!("{session}"));
        for n in 7..=21 {
            fields.push(format!("{n}"));
        }
        fields.push(format!("{starttime}"));
        // A few trailing fields, as the real file has.
        fields.push("0 0 0".into());
        fields.join(" ")
    }

    #[test]
    fn an_ordinary_stat_yields_its_pgrp_session_and_starttime() {
        let stat = parse_stat(&stat_line("sshd", 42, 42, 9999)).unwrap();
        assert_eq!(
            stat,
            Stat {
                pgrp: 42,
                session: 42,
                starttime: 9999
            }
        );
    }

    /// The whole reason this parser splits after the LAST `)`. A process can
    /// name itself anything; `comm` is not escaped in the file. Splitting the
    /// line on whitespace shifts every field after it, so a hostile — or merely
    /// unlucky — name silently returns another process's numbers.
    #[test]
    fn a_comm_containing_spaces_and_parens_does_not_shift_the_fields() {
        for comm in [
            "evil ) proc",
            "((nested))",
            ") ) )",
            "with spaces",
            "tab\there",
        ] {
            let stat = parse_stat(&stat_line(comm, 7, 8, 1234)).unwrap();
            assert_eq!(
                stat,
                Stat {
                    pgrp: 7,
                    session: 8,
                    starttime: 1234
                },
                "comm {comm:?} shifted the fields"
            );
        }
    }

    /// Naive splitting is what this guards against — assert it really would
    /// have been wrong, so the test cannot pass for the wrong reason.
    #[test]
    fn naive_whitespace_splitting_would_have_been_wrong() {
        let line = stat_line("evil ) proc", 7, 8, 1234);
        let naive: Vec<&str> = line.split_whitespace().collect();
        // Field 5 by naive counting is not 7 — the comm added two words.
        assert_ne!(naive.get(4).and_then(|f| f.parse::<i32>().ok()), Some(7));
    }

    #[test]
    fn a_truncated_or_malformed_stat_is_none_rather_than_a_wrong_answer() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("7 (sh) S 1 2").is_none());
        assert!(parse_stat("no parens here at all").is_none());
        // Present but non-numeric where a number belongs.
        assert!(parse_stat("7 (sh) S 1 x 8 9").is_none());
    }

    /// ESRCH is how `/proc` reports a process that exited between the open and
    /// the read. std gives errno 3 no named `ErrorKind`, so a `NotFound`-only
    /// test misses it and the teardown scan errors out exactly when processes
    /// are exiting fastest.
    #[test]
    fn a_vanished_process_is_recognised_from_either_errno() {
        assert!(is_gone(&io::Error::from_raw_os_error(ESRCH)));
        assert!(is_gone(&io::Error::from_raw_os_error(2))); // ENOENT
        assert_ne!(
            io::Error::from_raw_os_error(ESRCH).kind(),
            io::ErrorKind::NotFound,
            "if std ever maps ESRCH to NotFound this test is the place to notice"
        );
    }

    /// The faults that must NOT be read as an absence: an unreadable `/proc`
    /// entry is a reason to stop, not a reason to believe nothing is running.
    #[test]
    fn a_permission_or_io_error_is_not_an_absence() {
        assert!(!is_gone(&io::Error::from_raw_os_error(13))); // EACCES
        assert!(!is_gone(&io::Error::from_raw_os_error(5))); // EIO
    }

    /// A pid does not identify a process. The KILL that follows a TERM fires
    /// `stop-timeout` later, and in that window the target can die, be reaped,
    /// and have its pid recycled — so the KILL checks field 22 first. Run
    /// against this process, whose real starttime is readable.
    #[test]
    fn a_recycled_pid_is_not_the_same_process() {
        let me = std::process::id() as i32;
        let Some(stat) = stat_of(me).unwrap() else {
            return; // no /proc in this sandbox; the recipe leg covers it
        };
        assert!(is_same_process(me, stat.starttime).unwrap());
        assert!(
            !is_same_process(me, stat.starttime.wrapping_add(1)).unwrap(),
            "a different start time is a different process, whatever the pid"
        );
    }

    #[test]
    fn a_starttime_of_zero_parses_rather_than_reading_as_absent() {
        let line = stat_line("sh", 1, 1, 0);
        assert_eq!(parse_stat(&line).unwrap().starttime, 0);
    }
}
